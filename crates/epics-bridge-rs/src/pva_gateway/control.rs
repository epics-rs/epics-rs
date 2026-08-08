//! runtime-control PVs exposed under a configurable prefix.
//!
//! Mirrors `pva2pva` `ServerConfig::control_prefix` semantics — when an
//! operator sets a non-empty prefix on the gateway, a small set of
//! dynamic diagnostic PVs is added alongside the proxied namespace so
//! `pvget <prefix>:cacheSize` etc. return live state without going
//! through any upstream IOC.
//!
//! ## Read-only diagnostic PVs
//!
//! All names use the configurable prefix (no default — the feature is
//! opt-in via [`super::gateway::PvaGatewayConfig::control_prefix`]):
//!
//! | PV | Type | Description |
//! |----|------|-------------|
//! | `<prefix>:cacheSize` | Long | Live count of cached upstream entries |
//! | `<prefix>:upstreamCount` | Long | Count of upstream cache layers (shared + per-credential), distinct from channel count |
//! | `<prefix>:liveSubscribers` | Long | Current bridge-task count (downstream sub bridges) |
//! | `<prefix>:report` | String | Multi-line diagnostic snapshot |
//!
//! ## B6: writable control via RPC
//!
//! Three additional PVs accept **RPC** calls (not PUT) that mutate
//! gateway state. Every RPC is gated by the gateway's ACF policy
//! ([`ControlSource::with_control_acf`]): a flush/drop/reload is allowed
//! iff the configured control ACF grants WRITE to the caller's
//! `(host, account, method, roles)` under the control ASG — the same
//! [`AccessGate`] machinery the proxied namespace uses, not a bespoke
//! host/account allow-list. With no control ACF configured the writable
//! surface is closed (every caller denied), so an operator MUST opt in
//! explicitly before the control surface does anything destructive.
//!
//! | PV | RPC args | Effect |
//! |----|----------|--------|
//! | `<prefix>:flush` | none | Drop every cached upstream entry; returns `removed` count |
//! | `<prefix>:drop` | `pv` (string) | Drop one cache entry by exact name; returns `dropped` bool |
//! | `<prefix>:reload` | optional `path` (string) | Re-parse the ACF file and hot-swap the gateway-side policy |
//!
//! The RPC reply is a gateway-private (non-normative) structure with an
//! empty structure ID carrying the operation result (`value`) plus a
//! human-readable `message`. The top-level `message` is a gateway
//! extension absent from `epics:nt/NTScalar:1.0`, so the reply does not
//! claim that normative ID. RPC is used (not PUT) because each control
//! operation takes structured arguments (`drop`'s target `pv`,
//! `reload`'s ACF `path`) and returns a structured result; PUT has no
//! request-argument channel. Authorization still flows through the same
//! per-op AccessGate WRITE check the wire layer performs for PUT —
//! `rpc_checked` evaluates the control ACF against the peer's
//! `(host, account, method, roles)`.

// RTEMS-EXEC-MODEL-ALLOW(16): checked to pass feature-ON under
// --features rtems-exec-model,pva-gateway (the gateway's spawns/timers ride the
// runtime::task seam). The default feature-ON gate omits `pva-gateway`, so re-run
// that combo when touching this module.

use std::sync::Arc;

use epics_base_rs::server::access_security::{AccessGate, AccessSecurityConfig, AsgAslResolver};
use epics_pva_rs::nt::NTScalar;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, RpcReply, ScalarType, ScalarValue};
use epics_pva_rs::server::native_source::AcfCell;
use epics_pva_rs::server_native::source::{
    AccessChecked, ChannelContext, ChannelSource, MonitorStream, OpError,
};
use tokio::sync::mpsc;

use super::source::GatewayChannelSource;

/// Diagnostic + control PV source that lives behind the gateway's
/// `control_prefix`. Owned by the gateway alongside the proxy
/// `GatewayChannelSource`; both are registered into a
/// `CompositeSource` and dispatched in priority order.
#[derive(Clone)]
pub struct ControlSource {
    prefix: String,
    /// Every upstream `GatewayChannelSource` this control surface
    /// administers. A single-tenant gateway has exactly one; a
    /// multi-tenant downstream registers one per upstream label
    /// ([`Self::with_source`]). Diagnostics (`cacheSize` /
    /// `liveSubscribers` / `report`) aggregate across ALL of them and
    /// `flush` / `drop` reach ALL of them — pre-fix the control source
    /// held only the first upstream, so `<prefix>:cacheSize` could
    /// report zero and `flush` / `drop` could silently miss every
    /// non-first tenant. pva2pva's operator
    /// cache/status paths are client-aware, iterating every configured
    /// client (`p2pApp/server.cpp:102-138`, `:158-310`).
    gateway_sources: Vec<GatewayChannelSource>,
    /// B6: ACF authorization gate for the writable control RPCs. `None`
    /// = the writable surface is closed (every flush/drop/reload
    /// denied) — the safe default that reproduces the original
    /// deny-all posture without a bespoke account/host predicate.
    /// `Some(gate)` = a flush/drop/reload is allowed iff the configured
    /// control ACF grants WRITE to the caller, evaluated through the
    /// same [`AccessGate::required`] / ASG machinery the proxied
    /// namespace uses ([`GatewayChannelSource`]'s `build_gate`).
    /// Installed at startup from `PvaGatewayConfig::control_acf_path`
    /// via [`Self::with_control_acf`]; the gate's cell is seeded with
    /// the policy, so its permissive-when-empty path is never the
    /// authority here — `None` is the only "no policy" state and it is
    /// closed.
    control_gate: Option<AccessGate>,
    /// B6: ACF file path used by the `<prefix>:reload` RPC when the
    /// caller does not supply an explicit `path` argument. `None`
    /// means "no default path configured" — `reload` then requires
    /// the `path` argument or fails with a clear error.
    acf_path: Option<String>,
}

/// ASG every writable control PV resolves to under the control gate.
/// A dedicated control ACF file is expected to grant WRITE here (its
/// `ASG(DEFAULT)`), independently of the proxied namespace's per-PV
/// ASGs. `check_with_roles` falls back to `DEFAULT` for an unknown ASG,
/// so this also matches a minimal single-`ASG(DEFAULT)` file.
const CONTROL_ASG: &str = "DEFAULT";

impl ControlSource {
    pub fn new(prefix: impl Into<String>, gateway_source: GatewayChannelSource) -> Self {
        Self {
            prefix: prefix.into(),
            gateway_sources: vec![gateway_source],
            // Closed-by-default: with no control ACF the writable
            // surface denies every caller (see `control_gate`).
            control_gate: None,
            acf_path: None,
        }
    }

    /// Register an additional upstream `GatewayChannelSource` under this
    /// control surface. A multi-tenant downstream calls this once per
    /// upstream label beyond the first so diagnostics and cache
    /// administration span every tenant, not just the first.
    pub fn with_source(mut self, gateway_source: GatewayChannelSource) -> Self {
        self.gateway_sources.push(gateway_source);
        self
    }

    /// B6: install the ACF policy that gates the writable control RPCs
    /// (`flush` / `drop` / `reload`). A flush/drop/reload is then
    /// allowed iff this ACF grants WRITE to the caller's
    /// `(host, account, method, roles)` under the control ASG
    /// (`CONTROL_ASG`) — the same [`AccessGate`] machinery the
    /// proxied namespace uses, NOT a host/account allow-list. Without
    /// this the writable surface stays closed (every RPC denied).
    pub fn with_control_acf(mut self, cfg: AccessSecurityConfig) -> Self {
        let acf: AcfCell = epics_base_rs::server::access_security::new_acf_cell(Some(cfg));
        self.control_gate = Some(AccessGate::required(acf, Self::control_asg_resolver()));
        self
    }

    /// ASG/ASL resolver for the control gate: every writable control PV
    /// resolves to `CONTROL_ASG` with ASL 0, so an operator grants
    /// WRITE to it independently of the proxied namespace's per-PV ASGs.
    fn control_asg_resolver() -> AsgAslResolver {
        Arc::new(|_pv_name| Box::pin(async { (CONTROL_ASG.to_string(), 0u8) }))
    }

    /// B6: set the default ACF file path the `<prefix>:reload` RPC
    /// re-parses when the caller omits an explicit `path` argument.
    pub fn with_acf_path(mut self, path: impl Into<String>) -> Self {
        self.acf_path = Some(path.into());
        self
    }

    /// Read-only diagnostic PV names.
    fn diag_pv_names(&self) -> [String; 4] {
        [
            format!("{}:cacheSize", self.prefix),
            format!("{}:upstreamCount", self.prefix),
            format!("{}:liveSubscribers", self.prefix),
            format!("{}:report", self.prefix),
        ]
    }

    /// B6: writable control PV names (RPC targets).
    fn control_pv_names(&self) -> [String; 3] {
        [
            format!("{}:flush", self.prefix),
            format!("{}:drop", self.prefix),
            format!("{}:reload", self.prefix),
        ]
    }

    /// Build the NTScalar value for a Long counter via the shared
    /// [`NTScalar`] builder so the advertised `epics:nt/NTScalar:1.0`
    /// structure carries the mandatory `alarm` and `timeStamp` members
    /// alongside `value`, matching pvxs `NTScalar::build()`
    /// (`nt.cpp:44-53`). A strict NT client that selects the normative
    /// layout by structure ID then finds every member it expects; the
    /// ID and the shape no longer disagree.
    fn nt_scalar_long(v: i64) -> PvField {
        let mut value = NTScalar::new(ScalarType::Long).create();
        if let PvField::Structure(s) = &mut value {
            s.set("value", PvField::Scalar(ScalarValue::Long(v)));
        }
        value
    }

    fn nt_scalar_long_desc() -> FieldDesc {
        NTScalar::new(ScalarType::Long).build()
    }

    fn nt_scalar_string(v: String) -> PvField {
        let mut value = NTScalar::new(ScalarType::String).create();
        if let PvField::Structure(s) = &mut value {
            s.set("value", PvField::Scalar(ScalarValue::String(v.into())));
        }
        value
    }

    fn nt_scalar_string_desc() -> FieldDesc {
        NTScalar::new(ScalarType::String).build()
    }

    /// Per-cache cap on entry rows the `<prefix>:report` PV lists, so a
    /// gateway proxying tens of thousands of channels cannot produce a
    /// multi-megabyte report string. Omitted rows are summarized as a
    /// `(+N more)` tail.
    const REPORT_ROWS_PER_CACHE: usize = 64;

    /// Build the multi-line `<prefix>:report` snapshot.
    /// pva2pva's status RPC walks every
    /// configured client and lists its cached channels with per-channel
    /// state (`p2pApp/server.cpp:158-230`); the old single-line report
    /// collapsed everything to three counters (and aliased
    /// `upstreamCount` to `cacheSize`), so an operator could not see
    /// which channels were connected, how many downstream subscribers
    /// each carried, or whether the idle-eviction loop was running. This
    /// reproduces the per-channel detail across every tenant and cache
    /// layer, including the cleaner counters.
    async fn build_report(&self, cache_size: i64, upstream_count: i64, live_subs: i64) -> String {
        let mut out = format!(
            "cacheSize={cache_size} upstreamCount={upstream_count} liveSubscribers={live_subs} tenants={}\n",
            self.gateway_sources.len()
        );
        for (ti, src) in self.gateway_sources.iter().enumerate() {
            let caches = src.cache_status(Self::REPORT_ROWS_PER_CACHE).await;
            out.push_str(&format!("tenant[{ti}] caches={}\n", caches.len()));
            for (ci, cache) in caches.iter().enumerate() {
                out.push_str(&format!(
                    "  cache[{ci}] entries={} max={} cleanerRuns={} cleanerRemoved={}\n",
                    cache.total, cache.max_entries, cache.cleaner_runs, cache.cleaner_removed
                ));
                for e in &cache.entries {
                    out.push_str(&format!(
                        "    {} connected={} subscribers={} subscriptions={} dropPoke={}\n",
                        e.pv_name, e.connected, e.subscribers, e.subscriptions, e.drop_poke
                    ));
                }
                if cache.truncated > 0 {
                    out.push_str(&format!("    (+{} more)\n", cache.truncated));
                }
            }
        }
        out
    }

    /// B6: RPC reply descriptor — a gateway-private structure carrying a
    /// numeric `value` and a human-readable `message`. The top-level
    /// `message` member is a gateway extension with no equivalent in the
    /// normative `epics:nt/NTScalar:1.0` layout, so the reply uses an
    /// empty (anonymous) structure ID rather than claiming the NT ID:
    /// pvxs reserves the NTScalar ID for structures whose members match
    /// `NTScalar::build()` (`nt.cpp:44-53`), and a strict NT client must
    /// not be told this is one of them.
    fn control_reply_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Long)),
                ("message".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        }
    }

    /// B6: RPC reply value carrying a numeric result and a message.
    fn control_reply(value: i64, message: impl Into<String>) -> PvField {
        let mut s = PvStructure::new("");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Long(value))));
        // `message: impl Into<String>` yields a `String`; the NT value field
        // wants a `PvString` (PVA-89 byte-string newtype), so convert.
        let message: String = message.into();
        s.fields.push((
            "message".into(),
            PvField::Scalar(ScalarValue::String(message.into())),
        ));
        PvField::Structure(s)
    }

    /// True iff `name` is one of the read-only diagnostic PVs.
    fn is_diag(&self, name: &str) -> bool {
        self.diag_pv_names().iter().any(|n| n == name)
    }

    /// True iff `name` is one of the writable control RPC PVs.
    fn is_control(&self, name: &str) -> bool {
        self.control_pv_names().iter().any(|n| n == name)
    }

    fn matches(&self, name: &str) -> bool {
        self.is_diag(name) || self.is_control(name)
    }

    /// B6: pull a string argument out of an NTURI-style RPC request
    /// value. The request is a structure whose `query` sub-structure
    /// holds the named args; a bare top-level field is also accepted
    /// so `pvcall <pv> pv=X` and hand-built requests both work.
    fn rpc_string_arg(request_value: &PvField, arg: &str) -> Option<String> {
        fn scalar_string(f: &PvField) -> Option<String> {
            match f {
                PvField::Scalar(ScalarValue::String(s)) => Some(s.to_string()),
                _ => None,
            }
        }
        let PvField::Structure(root) = request_value else {
            return None;
        };
        // NTURI: look inside `query` first.
        if let Some((_, PvField::Structure(query))) = root.fields.iter().find(|(n, _)| n == "query")
        {
            if let Some((_, f)) = query.fields.iter().find(|(n, _)| n == arg) {
                if let Some(s) = scalar_string(f) {
                    return Some(s);
                }
            }
        }
        // Fallback: bare top-level field.
        root.fields
            .iter()
            .find(|(n, _)| n == arg)
            .and_then(|(_, f)| scalar_string(f))
    }

    /// B6: execute one writable control RPC. `name` is the control PV
    /// (already confirmed to be a control PV by the caller).
    async fn run_control_rpc(
        &self,
        name: &str,
        request_value: &PvField,
    ) -> Result<(FieldDesc, PvField), String> {
        let names = self.control_pv_names();
        if name == names[0] {
            // <prefix>:flush — drop every cached upstream entry across
            // the shared cache AND all per-credential caches, for EVERY
            // upstream tenant of this downstream (not just the first).
            let mut removed = 0i64;
            for src in &self.gateway_sources {
                removed += src.flush_all_caches().await as i64;
            }
            tracing::info!(
                gateway_control = %name,
                removed,
                "pva-gateway: operator flushed channel cache via RPC"
            );
            Ok((
                Self::control_reply_desc(),
                Self::control_reply(removed, format!("flushed {removed} cache entries")),
            ))
        } else if name == names[1] {
            // <prefix>:drop — drop one entry by name.
            let target = Self::rpc_string_arg(request_value, "pv").ok_or_else(|| {
                "drop RPC requires a string 'pv' argument naming the cache entry".to_string()
            })?;
            if target.is_empty() {
                return Err("drop RPC 'pv' argument must not be empty".to_string());
            }
            // Drop from the shared cache AND every per-credential cache,
            // for EVERY upstream tenant — a PV proxied through a
            // non-first upstream must still be reachable by `drop`.
            let mut dropped = false;
            for src in &self.gateway_sources {
                dropped |= src.drop_entry_all_caches(&target).await;
            }
            tracing::info!(
                gateway_control = %name,
                pv = %target,
                dropped,
                "pva-gateway: operator dropped cache entry via RPC"
            );
            let msg = if dropped {
                format!("dropped cache entry '{target}'")
            } else {
                format!("cache entry '{target}' was not present")
            };
            Ok((
                Self::control_reply_desc(),
                Self::control_reply(i64::from(dropped), msg),
            ))
        } else {
            // <prefix>:reload — re-parse the ACF file and hot-swap.
            let path = Self::rpc_string_arg(request_value, "path")
                .filter(|p| !p.is_empty())
                .or_else(|| self.acf_path.clone())
                .ok_or_else(|| {
                    "reload RPC requires a 'path' argument (no default ACF path \
                     configured on this gateway)"
                        .to_string()
                })?;
            // Async file read — `std::fs` would block the tokio
            // worker thread for the duration of the disk read inside
            // this RPC handler.
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("reload: cannot read ACF file '{path}': {e}"))?;
            let cfg = epics_base_rs::server::access_security::parse_acf(&content)
                .map_err(|e| format!("reload: cannot parse ACF file '{path}': {e}"))?;
            // Hot-swap the policy on EVERY upstream tenant of this
            // downstream, not just the first.
            for src in &self.gateway_sources {
                src.set_acf(Some(cfg.clone())).await;
            }
            tracing::info!(
                gateway_control = %name,
                acf_path = %path,
                "pva-gateway: operator reloaded ACF policy via RPC"
            );
            Ok((
                Self::control_reply_desc(),
                Self::control_reply(0, format!("reloaded ACF policy from '{path}'")),
            ))
        }
    }
}

impl ChannelSource for ControlSource {
    async fn list_pvs(&self) -> Vec<String> {
        let mut out = self.diag_pv_names().to_vec();
        out.extend(self.control_pv_names());
        out
    }

    async fn has_pv(&self, name: &str) -> bool {
        self.matches(name)
    }

    async fn get_introspection(&self, name: &str) -> Option<FieldDesc> {
        if self.is_control(name) {
            // Control PVs serve their RPC-reply shape for GET-INIT so
            // a client that introspects before calling sees the
            // result structure.
            return Some(Self::control_reply_desc());
        }
        if !self.is_diag(name) {
            return None;
        }
        if name.ends_with(":report") {
            Some(Self::nt_scalar_string_desc())
        } else {
            Some(Self::nt_scalar_long_desc())
        }
    }

    async fn get_value(&self, name: &str) -> Option<PvField> {
        if self.is_control(name) {
            // Control PVs are RPC targets; a plain GET reports the
            // last-known operation shape with a hint message rather
            // than performing the mutation.
            return Some(Self::control_reply(
                0,
                "control PV — invoke via RPC (pvcall), not GET/PUT",
            ));
        }
        if !self.is_diag(name) {
            return None;
        }
        // Live snapshot: pulled at every GET. Cheap — no upstream
        // round-trip, just a HashMap len() under a tokio::Mutex plus
        // an atomic load for the bridge-task count.
        // Aggregate across EVERY upstream tenant, and within each across
        // the shared cache AND every per-credential cache, so the
        // operator does not see zero while a non-first tenant's
        // credentialed upstream monitors remain alive.
        let mut cache_size = 0i64;
        let mut upstream_count = 0i64;
        let mut live_subs = 0i64;
        for src in &self.gateway_sources {
            cache_size += src.total_cached_entry_count().await as i64;
            upstream_count += src.upstream_cache_count() as i64;
            live_subs += src.live_subscribers() as i64;
        }

        if name.ends_with(":cacheSize") {
            Some(Self::nt_scalar_long(cache_size))
        } else if name.ends_with(":upstreamCount") {
            // Distinct from cacheSize: the count of upstream cache layers
            // (shared + per-credential, summed over tenants), NOT the
            // channel count. pva2pva reports these separately
            // (`p2pApp/server.cpp:158-175`); the old alias collapsed them
            // so an operator could not tell "many channels, one upstream"
            // from "many credentialed upstreams".
            Some(Self::nt_scalar_long(upstream_count))
        } else if name.ends_with(":liveSubscribers") {
            Some(Self::nt_scalar_long(live_subs))
        } else if name.ends_with(":report") {
            let report = self
                .build_report(cache_size, upstream_count, live_subs)
                .await;
            Some(Self::nt_scalar_string(report))
        } else {
            None
        }
    }

    async fn is_writable(&self, _name: &str) -> bool {
        // No PV here accepts PUT. The diagnostic PVs are read-only;
        // the control PVs are RPC targets. An attempt to PUT any of
        // them surfaces `is_writable=false` and the server rejects it
        // with the standard "channel not writable" status.
        false
    }

    async fn put_value(&self, name: &str, _value: PvField) -> Result<(), OpError> {
        // Wrong access method / read-only property — operational, not an
        // authorization denial (Failed bucket).
        if self.is_control(name) {
            Err(OpError::failed(format!(
                "control PV '{name}' is invoked via RPC (pvcall), not PUT"
            )))
        } else {
            Err(OpError::failed("control PVs are read-only"))
        }
    }

    /// Reject PROCESS. The `ChannelSource` default `process` returns
    /// `Ok(())` — for this source that would silently swallow a PVA
    /// PROCESS (`caput -c` / `pvcall .PROC`) and falsely report
    /// success. Neither the diagnostic PVs (read-only) nor the
    /// control PVs (RPC targets) have processing semantics, so a
    /// PROCESS is refused the same way `put_value` refuses a PUT.
    /// `process_checked`'s default delegates here after its WRITE
    /// gate, so this one override covers both entry points.
    async fn process(&self, name: &str) -> Result<(), OpError> {
        // All operational (wrong method / read-only / not found) — Failed.
        if self.is_control(name) {
            Err(OpError::failed(format!(
                "control PV '{name}' is invoked via RPC (pvcall), not PROCESS"
            )))
        } else if self.is_diag(name) {
            Err(OpError::failed(format!(
                "'{name}' is a read-only diagnostic PV — PROCESS not supported"
            )))
        } else {
            Err(OpError::failed(format!("unknown control PV '{name}'")))
        }
    }

    /// B6: writable control surface. RPC is gated by the operator
    /// credential predicate; `rpc` (the ctx-less path) carries no
    /// identity and so can never be allowed — only `rpc_checked`,
    /// which threads `ChannelContext`, can pass the credential check.
    async fn rpc(
        &self,
        name: &str,
        _request_desc: FieldDesc,
        _request_value: PvField,
    ) -> Result<RpcReply, OpError> {
        if self.is_control(name) {
            // No ctx — no credentials to check. Refusing a mutation for
            // lack of authentication is an authorization decision (Denied).
            Err(OpError::denied(format!(
                "control RPC '{name}' requires an authenticated request"
            )))
        } else if self.is_diag(name) {
            // Wrong target kind / not found — operational (Failed).
            Err(OpError::failed(format!(
                "'{name}' is a read-only diagnostic PV, not an RPC"
            )))
        } else {
            Err(OpError::failed(format!("unknown control PV '{name}'")))
        }
    }

    /// B6: credentialed control RPC. `ctx` carries the downstream
    /// peer's `(host, account, method, roles)`; the configured control
    /// ACF decides whether the mutation is allowed.
    async fn rpc_checked(
        &self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> Result<RpcReply, OpError> {
        let name = checked.pv_name().to_string();
        if !self.is_control(&name) {
            // Diagnostic PVs and unknown names fall back to the
            // ctx-less path's error messages.
            return self.rpc(&name, request_desc, request_value).await;
        }
        // Authorization: the writable control surface is gated by the
        // configured control ACF. No policy configured ⇒ closed (the
        // safe default; reproduces the original deny-all). With a
        // policy, the caller must hold WRITE under the control ASG —
        // evaluated through the same AccessGate the proxied namespace
        // uses, so there is no bespoke host/account allow-list. The
        // passed `checked` was minted by this source's permissive
        // (diagnostic) gate, so it is NOT consulted for the WRITE
        // decision; the control gate is the sole authority.
        let granted = match &self.control_gate {
            None => false,
            Some(gate) => gate
                .check_with_roles(
                    name.clone(),
                    &ctx.creds.host,
                    &ctx.creds.account,
                    &ctx.creds.roles,
                    &ctx.creds.method,
                    &ctx.creds.authority,
                )
                .await
                .allows_write(),
        };
        if !granted {
            tracing::warn!(
                gateway_control = %name,
                account = %ctx.creds.account,
                method = %ctx.creds.method,
                host = %ctx.creds.host,
                "pva-gateway: control RPC denied — no WRITE grant under the control ACF"
            );
            return Err(OpError::denied(format!(
                "control RPC '{name}' denied: {account}/{method} from {host} \
                 holds no WRITE grant under the gateway control ACF",
                account = ctx.creds.account,
                method = ctx.creds.method,
                host = ctx.creds.host,
            )));
        }
        // run_control_rpc validates arguments and executes the admin
        // command; its errors are operational (bad argument, exec
        // failure), so map them into the Failed bucket.
        self.run_control_rpc(&name, &request_value)
            .await
            .map(RpcReply::from)
            .map_err(OpError::failed)
    }

    async fn subscribe(&self, name: &str) -> Option<MonitorStream<PvField>> {
        // Control PVs are RPC targets — a monitor against one would
        // never see an event. Only diagnostic PVs get a live channel.
        if !self.is_diag(name) {
            return None;
        }
        // Control PVs are snapshots, but a `pvmonitor` against one of
        // them needs a live channel — without one the server emits
        // the initial value, sees rx close, and sends MONITOR FINISH
        // (subcmd 0x10), which pvxs interprets as "channel closed"
        // and reconnect-spins. Spawn a 1 Hz refresh task that holds
        // the tx alive and pushes the latest snapshot whenever a
        // counter changes. The task exits when the receiver is
        // dropped (downstream client unsubscribed).
        let (tx, rx) = mpsc::channel::<PvField>(4);
        let me = self.clone();
        let pv_name = name.to_string();
        epics_base_rs::runtime::task::spawn(async move {
            let mut tick =
                epics_base_rs::runtime::task::interval(std::time::Duration::from_secs(1));
            tick.tick().await; // skip the immediate fire — server emits
            // initial via get_value.
            let mut last: Option<PvField> = None;
            loop {
                tick.tick().await;
                let snapshot = me.get_value(&pv_name).await;
                if let Some(value) = snapshot {
                    let changed = match &last {
                        Some(prev) => prev != &value,
                        None => true,
                    };
                    if changed {
                        if tx.send(value.clone()).await.is_err() {
                            break;
                        }
                        last = Some(value);
                    }
                }
            }
        });
        Some(MonitorStream::Channel(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pva_gateway::channel_cache::{ChannelCache, DEFAULT_CLEANUP_INTERVAL};
    use epics_pva_rs::client::PvaClient;
    use epics_pva_rs::server_native::source::OpErrorKind;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn make_source() -> GatewayChannelSource {
        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(client, DEFAULT_CLEANUP_INTERVAL);
        GatewayChannelSource::new(cache)
    }

    /// Control ACF granting WRITE to everyone (the old allow-all
    /// predicate's ACF equivalent).
    fn allow_all_acf() -> AccessSecurityConfig {
        epics_base_rs::server::access_security::parse_acf(
            "ASG(DEFAULT) {\n  RULE(1, READ)\n  RULE(1, WRITE)\n}\n",
        )
        .unwrap()
    }

    /// Control ACF granting WRITE only to account `ops` (the old
    /// `c.account == "ops"` predicate's ACF equivalent).
    fn ops_only_acf() -> AccessSecurityConfig {
        epics_base_rs::server::access_security::parse_acf(
            "UAG(operators) { ops }\nASG(DEFAULT) {\n  RULE(1, READ)\n  RULE(1, WRITE) { UAG(operators) }\n}\n",
        )
        .unwrap()
    }

    fn ctx(account: &str, method: &str) -> ChannelContext {
        ChannelContext {
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5075),
            creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
                account: account.into(),
                method: method.into(),
                host: "localhost".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        }
    }

    fn nturi_with_arg(arg: &str, value: &str) -> PvField {
        let mut query = PvStructure::new("");
        query.fields.push((
            arg.into(),
            PvField::Scalar(ScalarValue::String(value.into())),
        ));
        let mut root = PvStructure::new("epics:nt/NTURI:1.0");
        root.fields
            .push(("query".into(), PvField::Structure(query)));
        PvField::Structure(root)
    }

    fn reply_message(reply: &PvField) -> String {
        let PvField::Structure(s) = reply else {
            panic!("reply not a structure");
        };
        match s.fields.iter().find(|(n, _)| n == "message") {
            Some((_, PvField::Scalar(ScalarValue::String(m)))) => m.to_string(),
            _ => panic!("reply has no message field"),
        }
    }

    fn reply_value(reply: &PvField) -> i64 {
        let PvField::Structure(s) = reply else {
            panic!("reply not a structure");
        };
        match s.fields.iter().find(|(n, _)| n == "value") {
            Some((_, PvField::Scalar(ScalarValue::Long(v)))) => *v,
            _ => panic!("reply has no value field"),
        }
    }

    fn reply_string(reply: &PvField) -> String {
        let PvField::Structure(s) = reply else {
            panic!("reply not a structure");
        };
        match s.fields.iter().find(|(n, _)| n == "value") {
            Some((_, PvField::Scalar(ScalarValue::String(v)))) => v.to_string(),
            _ => panic!("reply has no string value field"),
        }
    }

    /// Helper: mint an AccessChecked token via an Open gate so tests
    /// can exercise `rpc_checked` without an ACF.
    async fn checked(pv: &str) -> AccessChecked {
        epics_base_rs::server::access_security::AccessGate::open()
            .check(pv, "localhost", "ops", "ca", "")
            .await
    }

    /// MINOR regression: `ControlSource` must reject a PVA PROCESS.
    /// The `ChannelSource` default `process` returns `Ok(())`, which
    /// would silently swallow a `caput -c` / `pvcall .PROC` and
    /// falsely report success. Neither the diagnostic PVs nor the
    /// control PVs have processing semantics — PROCESS is refused the
    /// same way `put_value` refuses a PUT. `process_checked`'s default
    /// delegates to `process` after its WRITE gate, so the single
    /// `process` override covers both entry points.
    /// Control diagnostics and cache admin
    /// must span EVERY upstream tenant of a multi-tenant downstream,
    /// not just the first. Only the SECOND tenant's cache is populated
    /// here; `cacheSize`, `drop`, and `flush` must all reach it.
    #[tokio::test]
    async fn multi_tenant_control_spans_non_first_tenant() {
        let s0 = make_source();
        let s1 = make_source();
        // Populate ONLY the second tenant's shared cache.
        s1.cache().insert_test_entry("B:PV").await;
        s1.cache().insert_test_entry("B:PV2").await;
        let ctrl = ControlSource::new("gw", s0)
            .with_source(s1)
            .with_control_acf(allow_all_acf());

        let empty_req = || FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![],
        };

        // cacheSize must aggregate the non-first tenant's two entries.
        let v = ctrl.get_value("gw:cacheSize").await.expect("cacheSize");
        assert_eq!(
            reply_value(&v),
            2,
            "cacheSize must aggregate non-first tenant"
        );

        // drop must reach the second tenant.
        let (_d, reply) = ctrl
            .rpc_checked(
                checked("gw:drop").await,
                empty_req(),
                nturi_with_arg("pv", "B:PV"),
                ctx("ops", "ca"),
            )
            .await
            .expect("drop reply")
            .into_value()
            .expect("value reply");
        assert_eq!(
            reply_value(&reply),
            1,
            "drop must reach the non-first tenant"
        );

        // flush must reach the second tenant (one entry remains).
        let (_d, reply) = ctrl
            .rpc_checked(
                checked("gw:flush").await,
                empty_req(),
                PvField::Structure(PvStructure::new("")),
                ctx("ops", "ca"),
            )
            .await
            .expect("flush reply")
            .into_value()
            .expect("value reply");
        assert_eq!(
            reply_value(&reply),
            1,
            "flush must reach the non-first tenant"
        );

        let v = ctrl.get_value("gw:cacheSize").await.expect("cacheSize");
        assert_eq!(reply_value(&v), 0, "drop + flush must empty all tenants");
    }

    /// `upstreamCount` must be distinct from
    /// `cacheSize` (the old code aliased them), and `:report` must carry
    /// per-channel detail plus the cleaner counters rather than three
    /// collapsed numbers.
    #[tokio::test]
    async fn status_report_distinguishes_upstream_count_and_lists_channels() {
        let s0 = make_source();
        let s1 = make_source();
        // Tenant 0 holds two channels; tenant 1 holds one. Both ride only
        // their shared cache (no per-credential layer), so there are two
        // upstream cache layers total but three cached channels.
        s0.cache().insert_test_entry("A:PV1").await;
        s0.cache().insert_test_entry("A:PV2").await;
        s1.cache().insert_test_entry("B:PV1").await;
        let ctrl = ControlSource::new("gw", s0).with_source(s1);

        let cache_size = reply_value(&ctrl.get_value("gw:cacheSize").await.expect("cacheSize"));
        let upstream = reply_value(
            &ctrl
                .get_value("gw:upstreamCount")
                .await
                .expect("upstreamCount"),
        );
        assert_eq!(cache_size, 3, "cacheSize counts every cached channel");
        assert_eq!(
            upstream, 2,
            "upstreamCount counts cache layers (one shared per tenant), not channels"
        );
        assert_ne!(
            cache_size, upstream,
            "upstreamCount must not alias cacheSize"
        );

        let report = reply_string(&ctrl.get_value("gw:report").await.expect("report"));
        // Header carries the distinct counters and tenant count.
        assert!(report.contains("cacheSize=3"), "report header: {report}");
        assert!(
            report.contains("upstreamCount=2"),
            "report header: {report}"
        );
        assert!(report.contains("tenants=2"), "report header: {report}");
        // Per-channel detail for every tenant, not just the first.
        assert!(report.contains("A:PV1"), "report lists tenant-0 channel");
        assert!(report.contains("A:PV2"), "report lists tenant-0 channel");
        assert!(
            report.contains("B:PV1"),
            "report lists non-first tenant channel"
        );
        // Cleaner counters are exposed (zero before any sweep, but present).
        assert!(
            report.contains("cleanerRuns="),
            "report exposes cleaner counters: {report}"
        );
        assert!(
            report.contains("subscribers="),
            "report exposes per-channel subscriber count"
        );
    }

    #[tokio::test]
    async fn control_process_is_rejected() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw);

        // Control PV (RPC target).
        let err = ctrl
            .process("gw:flush")
            .await
            .expect_err("PROCESS of a control PV must be rejected");
        assert!(
            err.message.contains("not PROCESS"),
            "control PV reason: {err:?}"
        );

        // Diagnostic PV (read-only).
        let err = ctrl
            .process("gw:cacheSize")
            .await
            .expect_err("PROCESS of a diagnostic PV must be rejected");
        assert!(
            err.message.contains("PROCESS not supported"),
            "diag PV reason: {err:?}"
        );

        // process_checked (WRITE-granted token) still rejects because
        // the default delegates to the overridden `process`.
        let err = ctrl
            .process_checked(checked("gw:flush").await, ctx("ops", "ca"))
            .await
            .expect_err("process_checked must reject even with a WRITE token");
        assert!(
            err.message.contains("not PROCESS"),
            "process_checked reason: {err:?}"
        );
    }

    #[tokio::test]
    async fn control_rpc_denied_when_no_control_acf() {
        let gw = make_source();
        // No control ACF configured ⇒ writable surface closed-by-default.
        let ctrl = ControlSource::new("gw", gw);
        let res = ctrl
            .rpc_checked(
                checked("gw:flush").await,
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                PvField::Structure(PvStructure::new("")),
                ctx("ops", "ca"),
            )
            .await;
        assert!(
            res.is_err(),
            "closed-by-default (no control ACF) must reject control RPC"
        );
        let err = res.unwrap_err();
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "credential refusal must classify as Denied: {err:?}"
        );
        assert!(
            err.message
                .contains("no WRITE grant under the gateway control ACF")
        );
    }

    #[tokio::test]
    async fn control_rpc_ctxless_path_always_refused() {
        let gw = make_source();
        // Even with an allow-all predicate the ctx-less `rpc` path
        // must refuse — it carries no credentials.
        let ctrl = ControlSource::new("gw", gw).with_control_acf(allow_all_acf());
        let res = ctrl
            .rpc(
                "gw:flush",
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                PvField::Structure(PvStructure::new("")),
            )
            .await;
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert_eq!(
            err.kind,
            OpErrorKind::Denied,
            "unauthenticated refusal must classify as Denied: {err:?}"
        );
        assert!(err.message.contains("requires an authenticated request"));
    }

    #[tokio::test]
    async fn flush_rpc_clears_cache_for_authorised_operator() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw).with_control_acf(ops_only_acf());
        let (desc, reply) = ctrl
            .rpc_checked(
                checked("gw:flush").await,
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                PvField::Structure(PvStructure::new("")),
                ctx("ops", "ca"),
            )
            .await
            .expect("authorised operator flush must succeed")
            .into_value()
            .expect("value reply");
        assert!(matches!(desc, FieldDesc::Structure { .. }));
        // Empty cache → 0 removed.
        assert_eq!(reply_value(&reply), 0);
        assert!(reply_message(&reply).contains("flushed"));
    }

    #[tokio::test]
    async fn flush_rpc_denied_for_unlisted_account() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw).with_control_acf(ops_only_acf());
        let res = ctrl
            .rpc_checked(
                checked("gw:flush").await,
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                PvField::Structure(PvStructure::new("")),
                ctx("intruder", "ca"),
            )
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn drop_rpc_requires_pv_argument() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw).with_control_acf(allow_all_acf());
        let res = ctrl
            .rpc_checked(
                checked("gw:drop").await,
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                PvField::Structure(PvStructure::new("")),
                ctx("ops", "ca"),
            )
            .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().message.contains("'pv' argument"));
    }

    #[tokio::test]
    async fn drop_rpc_reports_missing_entry() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw).with_control_acf(allow_all_acf());
        let (_desc, reply) = ctrl
            .rpc_checked(
                checked("gw:drop").await,
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                nturi_with_arg("pv", "NO:SUCH:PV"),
                ctx("ops", "ca"),
            )
            .await
            .expect("drop of an absent entry still returns a reply")
            .into_value()
            .expect("value reply");
        // Not present → dropped=false → value 0.
        assert_eq!(reply_value(&reply), 0);
        assert!(reply_message(&reply).contains("was not present"));
    }

    #[tokio::test]
    async fn reload_rpc_without_path_or_default_fails() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw).with_control_acf(allow_all_acf());
        let res = ctrl
            .rpc_checked(
                checked("gw:reload").await,
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                PvField::Structure(PvStructure::new("")),
                ctx("ops", "ca"),
            )
            .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().message.contains("'path' argument"));
    }

    #[tokio::test]
    async fn reload_rpc_parses_acf_from_explicit_path() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw).with_control_acf(allow_all_acf());
        // Write a minimal valid ACF file.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pva_gw_b6_reload_{}.acf", std::process::id()));
        std::fs::write(
            &path,
            "ASG(DEFAULT) {\n  RULE(1, READ)\n  RULE(1, WRITE)\n}\n",
        )
        .unwrap();
        let (_desc, reply) = ctrl
            .rpc_checked(
                checked("gw:reload").await,
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                nturi_with_arg("path", path.to_str().unwrap()),
                ctx("ops", "ca"),
            )
            .await
            .expect("reload of a valid ACF must succeed")
            .into_value()
            .expect("value reply");
        assert!(reply_message(&reply).contains("reloaded ACF policy"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reload_rpc_uses_configured_default_path() {
        let gw = make_source();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pva_gw_b6_default_{}.acf", std::process::id()));
        std::fs::write(&path, "ASG(DEFAULT) {\n  RULE(1, READ)\n}\n").unwrap();
        let ctrl = ControlSource::new("gw", gw)
            .with_control_acf(allow_all_acf())
            .with_acf_path(path.to_str().unwrap());
        let res = ctrl
            .rpc_checked(
                checked("gw:reload").await,
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                PvField::Structure(PvStructure::new("")),
                ctx("ops", "ca"),
            )
            .await;
        assert!(res.is_ok(), "reload must use the configured default path");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reload_rpc_rejects_unparseable_acf() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw).with_control_acf(allow_all_acf());
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pva_gw_b6_bad_{}.acf", std::process::id()));
        std::fs::write(&path, "this is not valid ACF (((").unwrap();
        let res = ctrl
            .rpc_checked(
                checked("gw:reload").await,
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![],
                },
                nturi_with_arg("path", path.to_str().unwrap()),
                ctx("ops", "ca"),
            )
            .await;
        assert!(res.is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn diagnostic_pvs_remain_read_only() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw);
        assert!(ctrl.has_pv("gw:cacheSize").await);
        assert!(ctrl.has_pv("gw:report").await);
        assert!(!ctrl.is_writable("gw:cacheSize").await);
        // get_value still works on the diagnostic PVs.
        assert!(ctrl.get_value("gw:cacheSize").await.is_some());
        // list_pvs exposes both diagnostic and control PVs.
        let names = ctrl.list_pvs().await;
        assert!(names.contains(&"gw:cacheSize".to_string()));
        assert!(names.contains(&"gw:flush".to_string()));
        assert!(names.contains(&"gw:drop".to_string()));
        assert!(names.contains(&"gw:reload".to_string()));
    }

    /// Member names of a structure descriptor, in declaration order.
    fn desc_member_names(desc: &FieldDesc) -> Vec<String> {
        match desc {
            FieldDesc::Structure { fields, .. } => fields.iter().map(|(n, _)| n.clone()).collect(),
            other => panic!("expected structure descriptor, got {other:?}"),
        }
    }

    fn desc_struct_id(desc: &FieldDesc) -> String {
        match desc {
            FieldDesc::Structure { struct_id, .. } => struct_id.clone(),
            other => panic!("expected structure descriptor, got {other:?}"),
        }
    }

    /// A descriptor that advertises the normative `epics:nt/NTScalar:1.0`
    /// structure ID MUST carry the mandatory `alarm` and `timeStamp`
    /// members alongside `value`, matching pvxs `NTScalar::build()`
    /// (`nt.cpp:44-53`). A truncated value-only structure under that ID
    /// is a wire-schema contract violation for strict NT clients.
    #[tokio::test]
    async fn diagnostic_descriptors_are_full_ntscalar() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw);

        // Long counter diagnostics (cacheSize, upstreamCount, liveSubscribers).
        let long_desc = ctrl
            .get_introspection("gw:cacheSize")
            .await
            .expect("cacheSize introspection");
        assert_eq!(desc_struct_id(&long_desc), "epics:nt/NTScalar:1.0");
        let names = desc_member_names(&long_desc);
        assert!(names.contains(&"value".to_string()));
        assert!(
            names.contains(&"alarm".to_string()),
            "NTScalar diagnostic must carry alarm: {names:?}"
        );
        assert!(
            names.contains(&"timeStamp".to_string()),
            "NTScalar diagnostic must carry timeStamp: {names:?}"
        );

        // String diagnostic (report).
        let string_desc = ctrl
            .get_introspection("gw:report")
            .await
            .expect("report introspection");
        assert_eq!(desc_struct_id(&string_desc), "epics:nt/NTScalar:1.0");
        let names = desc_member_names(&string_desc);
        assert!(names.contains(&"alarm".to_string()));
        assert!(names.contains(&"timeStamp".to_string()));

        // The served value must match the descriptor: alarm + timeStamp
        // sub-structures present, not just `value`.
        let PvField::Structure(s) = ctrl
            .get_value("gw:cacheSize")
            .await
            .expect("cacheSize value")
        else {
            panic!("diagnostic value must be a structure");
        };
        assert_eq!(s.struct_id, "epics:nt/NTScalar:1.0");
        assert!(s.get_field("alarm").is_some(), "value must carry alarm");
        assert!(
            s.get_field("timeStamp").is_some(),
            "value must carry timeStamp"
        );
    }

    /// The control RPC reply carries a top-level `message` extension with
    /// no equivalent in the normative NTScalar layout, so it must NOT
    /// claim `epics:nt/NTScalar:1.0`: it is advertised as a gateway-private
    /// structure with an empty (anonymous) structure ID and exactly
    /// `[value, message]`.
    #[tokio::test]
    async fn control_reply_is_gateway_private_not_ntscalar() {
        let gw = make_source();
        let ctrl = ControlSource::new("gw", gw);
        let desc = ctrl
            .get_introspection("gw:flush")
            .await
            .expect("control PV introspection");
        assert_eq!(
            desc_struct_id(&desc),
            "",
            "control reply must not claim the normative NTScalar ID"
        );
        assert_eq!(
            desc_member_names(&desc),
            vec!["value".to_string(), "message".to_string()],
        );
        // The served GET value carries the same empty-ID shape.
        let PvField::Structure(s) = ctrl.get_value("gw:flush").await.expect("control GET value")
        else {
            panic!("control value must be a structure");
        };
        assert_eq!(s.struct_id, "");
    }
}
