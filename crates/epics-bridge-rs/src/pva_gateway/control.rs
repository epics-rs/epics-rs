//! G-G2: runtime-control PVs exposed under a configurable prefix.
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
//! | `<prefix>:upstreamCount` | Long | Alias of cacheSize (pva2pva-compat) |
//! | `<prefix>:liveSubscribers` | Long | Current bridge-task count (downstream sub bridges) |
//! | `<prefix>:report` | String | Multi-line diagnostic snapshot |
//!
//! ## B6: writable control via RPC
//!
//! Three additional PVs accept **RPC** calls (not PUT) that mutate
//! gateway state. Every RPC is gated by a credential predicate
//! ([`ControlSource::with_credential_check`]); the default predicate
//! denies every caller, so an operator MUST opt in explicitly before
//! the control surface does anything destructive.
//!
//! | PV | RPC args | Effect |
//! |----|----------|--------|
//! | `<prefix>:flush` | none | Drop every cached upstream entry; returns `removed` count |
//! | `<prefix>:drop` | `pv` (string) | Drop one cache entry by exact name; returns `dropped` bool |
//! | `<prefix>:reload` | optional `path` (string) | Re-parse the ACF file and hot-swap the gateway-side policy |
//!
//! The RPC reply is an `epics:nt/NTScalar:1.0` structure carrying the
//! operation result plus a human-readable `message`. RPC is used (not
//! PUT) because the wire layer routes RPC through `rpc_checked`, which
//! threads the downstream peer's `(account, method, host)` credentials
//! — PUT's type-state token carries only the resolved access level,
//! not the raw identity needed for the operator allow-list.

use std::sync::Arc;

use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::source::{AccessChecked, ChannelContext, ChannelSource};
use tokio::sync::mpsc;

use super::channel_cache::ChannelCache;
use super::source::GatewayChannelSource;

/// Operator credential predicate for the writable control RPCs (B6).
///
/// Called once per control RPC with the downstream peer's
/// `(account, method, host)`. Returns `true` to allow the mutation.
/// The default predicate (installed by [`ControlSource::new`]) denies
/// everyone — the writable surface is inert until an operator calls
/// [`ControlSource::with_credential_check`] with a real policy.
pub type CredentialCheck = Arc<dyn Fn(&ChannelContext) -> bool + Send + Sync>;

/// Diagnostic + control PV source that lives behind the gateway's
/// `control_prefix`. Owned by the gateway alongside the proxy
/// `GatewayChannelSource`; both are registered into a
/// `CompositeSource` and dispatched in priority order.
#[derive(Clone)]
pub struct ControlSource {
    prefix: String,
    cache: Arc<ChannelCache>,
    gateway_source: GatewayChannelSource,
    /// B6: credential predicate gating the writable RPCs. Defaults to
    /// deny-all.
    credential_check: CredentialCheck,
    /// B6: ACF file path used by the `<prefix>:reload` RPC when the
    /// caller does not supply an explicit `path` argument. `None`
    /// means "no default path configured" — `reload` then requires
    /// the `path` argument or fails with a clear error.
    acf_path: Option<String>,
}

impl ControlSource {
    pub fn new(
        prefix: impl Into<String>,
        cache: Arc<ChannelCache>,
        gateway_source: GatewayChannelSource,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            cache,
            gateway_source,
            // Deny-all default: the writable surface does nothing
            // until an operator installs a real predicate.
            credential_check: Arc::new(|_ctx| false),
            acf_path: None,
        }
    }

    /// B6: install the operator credential predicate that gates the
    /// writable control RPCs (`flush` / `drop` / `reload`). Without
    /// this the control RPCs reject every caller.
    pub fn with_credential_check(mut self, check: CredentialCheck) -> Self {
        self.credential_check = check;
        self
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

    /// Build the NTScalar-shaped value for a Long counter so PVA
    /// clients see the same structure regardless of which counter PV
    /// they ask for. We deliberately keep the field set minimal
    /// (`value` only — no alarm/timeStamp shells) so the descriptor
    /// is small and the encode path stays cheap when these PVs are
    /// polled at high cadence.
    fn nt_scalar_long(v: i64) -> PvField {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Long(v))));
        PvField::Structure(s)
    }

    fn nt_scalar_long_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Long))],
        }
    }

    fn nt_scalar_string(v: String) -> PvField {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::String(v))));
        PvField::Structure(s)
    }

    fn nt_scalar_string_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::String))],
        }
    }

    /// B6: RPC reply descriptor — an NTScalar Long `value` plus a
    /// human-readable `message` string.
    fn control_reply_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Long)),
                ("message".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        }
    }

    /// B6: RPC reply value carrying a numeric result and a message.
    fn control_reply(value: i64, message: impl Into<String>) -> PvField {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Long(value))));
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
                PvField::Scalar(ScalarValue::String(s)) => Some(s.clone()),
                _ => None,
            }
        }
        let PvField::Structure(root) = request_value else {
            return None;
        };
        // NTURI: look inside `query` first.
        if let Some((_, PvField::Structure(query))) =
            root.fields.iter().find(|(n, _)| n == "query")
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
            // <prefix>:flush — drop the whole cache.
            let removed = self.cache.flush().await as i64;
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
            let dropped = self.cache.drop_entry(&target).await;
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
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("reload: cannot read ACF file '{path}': {e}"))?;
            let cfg = epics_base_rs::server::access_security::parse_acf(&content)
                .map_err(|e| format!("reload: cannot parse ACF file '{path}': {e}"))?;
            self.gateway_source.set_acf(Some(cfg)).await;
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
        let cache_size = self.cache.entry_count().await as i64;
        let live_subs = self.gateway_source.live_subscribers() as i64;

        if name.ends_with(":cacheSize") || name.ends_with(":upstreamCount") {
            Some(Self::nt_scalar_long(cache_size))
        } else if name.ends_with(":liveSubscribers") {
            Some(Self::nt_scalar_long(live_subs))
        } else if name.ends_with(":report") {
            let report = format!(
                "cacheSize={cache_size} upstreamCount={cache_size} liveSubscribers={live_subs}"
            );
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

    async fn put_value(&self, name: &str, _value: PvField) -> Result<(), String> {
        if self.is_control(name) {
            Err(format!(
                "control PV '{name}' is invoked via RPC (pvcall), not PUT"
            ))
        } else {
            Err("control PVs are read-only".to_string())
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
    ) -> Result<(FieldDesc, PvField), String> {
        if self.is_control(name) {
            // No ctx — no credentials to check. Refuse rather than
            // run an unauthenticated mutation.
            Err(format!(
                "control RPC '{name}' requires an authenticated request"
            ))
        } else if self.is_diag(name) {
            Err(format!("'{name}' is a read-only diagnostic PV, not an RPC"))
        } else {
            Err(format!("unknown control PV '{name}'"))
        }
    }

    /// B6: credentialed control RPC. `ctx` carries the downstream
    /// peer's `(account, method, host)`; the operator credential
    /// predicate decides whether the mutation is allowed.
    async fn rpc_checked(
        &self,
        checked: AccessChecked,
        request_desc: FieldDesc,
        request_value: PvField,
        ctx: ChannelContext,
    ) -> Result<(FieldDesc, PvField), String> {
        let name = checked.pv_name().to_string();
        if !self.is_control(&name) {
            // Diagnostic PVs and unknown names fall back to the
            // ctx-less path's error messages.
            return self.rpc(&name, request_desc, request_value).await;
        }
        if !(self.credential_check)(&ctx) {
            tracing::warn!(
                gateway_control = %name,
                account = %ctx.account,
                method = %ctx.method,
                host = %ctx.host,
                "pva-gateway: control RPC denied — credential check failed"
            );
            return Err(format!(
                "control RPC '{name}' denied: {account}/{method} from {host} \
                 is not an authorised gateway operator",
                account = ctx.account,
                method = ctx.method,
                host = ctx.host,
            ));
        }
        self.run_control_rpc(&name, &request_value).await
    }

    async fn subscribe(&self, name: &str) -> Option<mpsc::Receiver<PvField>> {
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
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
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
        Some(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pva_gateway::channel_cache::DEFAULT_CLEANUP_INTERVAL;
    use epics_pva_rs::client::PvaClient;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn make_source() -> (Arc<ChannelCache>, GatewayChannelSource) {
        let client = Arc::new(PvaClient::builder().build());
        let cache = ChannelCache::new(client, DEFAULT_CLEANUP_INTERVAL);
        let gw = GatewayChannelSource::new(cache.clone());
        (cache, gw)
    }

    fn ctx(account: &str, method: &str) -> ChannelContext {
        ChannelContext {
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5075),
            account: account.into(),
            method: method.into(),
            host: "localhost".into(),
            authority: String::new(),
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
            Some((_, PvField::Scalar(ScalarValue::String(m)))) => m.clone(),
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

    /// Helper: mint an AccessChecked token via an Open gate so tests
    /// can exercise `rpc_checked` without an ACF.
    async fn checked(pv: &str) -> AccessChecked {
        epics_base_rs::server::access_security::AccessGate::open()
            .check(pv, "localhost", "ops", "ca", "")
            .await
    }

    #[tokio::test]
    async fn control_rpc_denied_without_credential_predicate() {
        let (cache, gw) = make_source();
        let ctrl = ControlSource::new("gw", cache, gw);
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
        assert!(res.is_err(), "deny-all default must reject control RPC");
        assert!(res.unwrap_err().contains("not an authorised gateway operator"));
    }

    #[tokio::test]
    async fn control_rpc_ctxless_path_always_refused() {
        let (cache, gw) = make_source();
        // Even with an allow-all predicate the ctx-less `rpc` path
        // must refuse — it carries no credentials.
        let ctrl = ControlSource::new("gw", cache, gw)
            .with_credential_check(Arc::new(|_| true));
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
        assert!(res.unwrap_err().contains("requires an authenticated request"));
    }

    #[tokio::test]
    async fn flush_rpc_clears_cache_for_authorised_operator() {
        let (cache, gw) = make_source();
        let ctrl = ControlSource::new("gw", cache, gw)
            .with_credential_check(Arc::new(|c| c.account == "ops"));
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
            .expect("authorised operator flush must succeed");
        assert!(matches!(desc, FieldDesc::Structure { .. }));
        // Empty cache → 0 removed.
        assert_eq!(reply_value(&reply), 0);
        assert!(reply_message(&reply).contains("flushed"));
    }

    #[tokio::test]
    async fn flush_rpc_denied_for_unlisted_account() {
        let (cache, gw) = make_source();
        let ctrl = ControlSource::new("gw", cache, gw)
            .with_credential_check(Arc::new(|c| c.account == "ops"));
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
        let (cache, gw) = make_source();
        let ctrl = ControlSource::new("gw", cache, gw)
            .with_credential_check(Arc::new(|_| true));
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
        assert!(res.unwrap_err().contains("'pv' argument"));
    }

    #[tokio::test]
    async fn drop_rpc_reports_missing_entry() {
        let (cache, gw) = make_source();
        let ctrl = ControlSource::new("gw", cache, gw)
            .with_credential_check(Arc::new(|_| true));
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
            .expect("drop of an absent entry still returns a reply");
        // Not present → dropped=false → value 0.
        assert_eq!(reply_value(&reply), 0);
        assert!(reply_message(&reply).contains("was not present"));
    }

    #[tokio::test]
    async fn reload_rpc_without_path_or_default_fails() {
        let (cache, gw) = make_source();
        let ctrl = ControlSource::new("gw", cache, gw)
            .with_credential_check(Arc::new(|_| true));
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
        assert!(res.unwrap_err().contains("'path' argument"));
    }

    #[tokio::test]
    async fn reload_rpc_parses_acf_from_explicit_path() {
        let (cache, gw) = make_source();
        let ctrl = ControlSource::new("gw", cache.clone(), gw)
            .with_credential_check(Arc::new(|_| true));
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
            .expect("reload of a valid ACF must succeed");
        assert!(reply_message(&reply).contains("reloaded ACF policy"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reload_rpc_uses_configured_default_path() {
        let (cache, gw) = make_source();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pva_gw_b6_default_{}.acf", std::process::id()));
        std::fs::write(&path, "ASG(DEFAULT) {\n  RULE(1, READ)\n}\n").unwrap();
        let ctrl = ControlSource::new("gw", cache, gw)
            .with_credential_check(Arc::new(|_| true))
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
        let (cache, gw) = make_source();
        let ctrl = ControlSource::new("gw", cache, gw)
            .with_credential_check(Arc::new(|_| true));
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
        let (cache, gw) = make_source();
        let ctrl = ControlSource::new("gw", cache, gw);
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
}
