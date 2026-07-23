//! multi-tenant PVA gateway.
//!
//! pva2pva-style "N upstream × M downstream" topology in a single
//! process. Each downstream `PvaServer` selects a subset of the
//! configured upstream `PvaClient`s and proxies to them in priority
//! order.
//!
//! ## When to use this
//!
//! Use [`MultiTenantPvaGateway`] when one process needs to bridge
//! multiple isolated PV namespaces — e.g. a site-wide gateway that
//! exposes both the experimental floor and the controls subnet to a
//! shared visitor network, while keeping their `EPICS_PVA_ADDR_LIST`s
//! separate. Use [`super::PvaGateway`] for the typical
//! one-upstream-cluster-behind-one-server case (which is most
//! deployments).
//!
//! ## Topology
//!
//! ```text
//!   ┌─ upstream A ─┐    ┌─ ChannelCache A ─┐
//!   │ PvaClient    │ ─▶ │ (its own client) │ ─┐
//!   └──────────────┘    └──────────────────┘  │
//!                                              ├──▶ ┌─ downstream X ─┐
//!   ┌─ upstream B ─┐    ┌─ ChannelCache B ─┐  │     │ PvaServer       │
//!   │ PvaClient    │ ─▶ │ (its own client) │ ─┤     │ CompositeSource │
//!   └──────────────┘    └──────────────────┘  │     └─────────────────┘
//!                                              │
//!                                              └──▶ ┌─ downstream Y ─┐
//!                                                   │ subset of      │
//!                                                   │ upstreams      │
//!                                                   └────────────────┘
//! ```
//!
//! ## Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use epics_bridge_rs::pva_gateway::multi_gateway::MultiTenantPvaGatewayBuilder;
//! use epics_pva_rs::client::PvaClient;
//! use epics_pva_rs::server_native::PvaServerConfig;
//!
//! # fn run() -> epics_bridge_rs::pva_gateway::error::GwResult<()> {
//! let upstream_a = Arc::new(PvaClient::builder().build());
//! let upstream_b = Arc::new(PvaClient::builder().build());
//!
//! let gw = MultiTenantPvaGatewayBuilder::new()
//!     .add_upstream("A", upstream_a)
//!     .add_upstream("B", upstream_b)
//!     // Visitor network sees only namespace A
//!     .add_downstream(
//!         "visitor",
//!         PvaServerConfig::default(),
//!         &["A"],
//!         Some("gw:A".to_string()),
//!     )
//!     // Operator subnet sees both A and B (B preferred via order)
//!     .add_downstream(
//!         "ops",
//!         PvaServerConfig::default(),
//!         &["B", "A"],
//!         Some("gw:ops".to_string()),
//!     )
//!     .start()?;
//! # Ok(()) }
//! ```

// RTEMS-EXEC-MODEL-ALLOW(1): checked to pass feature-ON under
// --features rtems-exec-model,pva-gateway (the gateway's spawns/timers ride the
// runtime::task seam). The default feature-ON gate omits `pva-gateway`, so re-run
// that combo when touching this module.

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::server_native::{CompositeSource, PvaServer, PvaServerConfig};

use super::channel_cache::{ChannelCache, DEFAULT_CLEANUP_INTERVAL};
use super::control::ControlSource;
use super::error::{GwError, GwResult};
use super::middleware::{AclConfig, AuditSink, NoopAudit, layer_access_control};
use super::source::GatewayChannelSource;

/// One upstream tenant — a `PvaClient` (with its own connection pool
/// and EPICS_PVA_ADDR_LIST scope) labelled for routing.
struct UpstreamTenant {
    label: String,
    client: Arc<PvaClient>,
}

/// One downstream tenant — its `PvaServer` config, the labelled list
/// of upstream tenants it proxies, an optional control_prefix, and its
/// per-tenant access policy (ACL / read-only / audit). The access
/// fields default to "permissive + no audit"; set them via
/// [`MultiTenantPvaGatewayBuilder::downstream_access`].
struct DownstreamSpec {
    label: String,
    config: PvaServerConfig,
    upstream_labels: Vec<String>,
    control_prefix: Option<String>,
    /// `None` ⇒ permissive [`AclConfig::default`].
    acl: Option<AclConfig>,
    /// Reject every PUT on this downstream when set.
    read_only: bool,
    /// `None` ⇒ [`NoopAudit`].
    audit: Option<Arc<dyn AuditSink>>,
    /// B6: control-RPC authorization ACF path. `None` ⇒ the writable
    /// control surface (`<prefix>:flush` / `:drop` / `:reload`) stays
    /// closed for this downstream; destructive controls are opt-in.
    control_acf_path: Option<String>,
    /// B6: default ACF path the `<prefix>:reload` RPC re-parses (for
    /// the PROXIED-PV policy) when the caller omits an explicit `path`.
    control_reload_acf_path: Option<String>,
}

/// Builder for [`MultiTenantPvaGateway`]. Add upstreams first, then
/// downstreams that reference them by label.
pub struct MultiTenantPvaGatewayBuilder {
    upstreams: Vec<UpstreamTenant>,
    downstreams: Vec<DownstreamSpec>,
    cleanup_interval: Duration,
    connect_timeout: Duration,
    max_cache_entries: usize,
    max_subscribers: usize,
}

impl Default for MultiTenantPvaGatewayBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiTenantPvaGatewayBuilder {
    pub fn new() -> Self {
        Self {
            upstreams: Vec::new(),
            downstreams: Vec::new(),
            cleanup_interval: DEFAULT_CLEANUP_INTERVAL,
            connect_timeout: Duration::from_secs(5),
            max_cache_entries: super::channel_cache::DEFAULT_MAX_ENTRIES,
            max_subscribers: 100_000,
        }
    }

    /// Register an upstream tenant. `label` must be unique across
    /// upstreams; downstreams reference it via [`Self::add_downstream`].
    pub fn add_upstream(mut self, label: impl Into<String>, client: Arc<PvaClient>) -> Self {
        self.upstreams.push(UpstreamTenant {
            label: label.into(),
            client,
        });
        self
    }

    /// Register a downstream tenant. `upstream_labels` lists the
    /// upstreams to proxy in priority order — earlier labels are tried
    /// first when a downstream search arrives. `control_prefix`, when
    /// `Some`, exposes diagnostic PVs scoped to *this* downstream only
    /// (each downstream's stats are independent).
    pub fn add_downstream(
        mut self,
        label: impl Into<String>,
        config: PvaServerConfig,
        upstream_labels: &[&str],
        control_prefix: Option<String>,
    ) -> Self {
        self.downstreams.push(DownstreamSpec {
            label: label.into(),
            config,
            upstream_labels: upstream_labels.iter().map(|s| (*s).to_string()).collect(),
            control_prefix,
            acl: None,
            read_only: false,
            audit: None,
            control_acf_path: None,
            control_reload_acf_path: None,
        });
        self
    }

    /// B6: configure the writable-control authorization for the
    /// most-recently-added downstream. `control_acf_path` gates the
    /// `<prefix>:flush` / `:drop` / `:reload` RPCs through the ACF it
    /// names (closed when `None` — destructive controls are opt-in);
    /// `control_reload_acf_path` is the `:reload` default path applied
    /// when the caller omits an explicit `path`. Panics if called
    /// before any [`Self::add_downstream`].
    pub fn downstream_control_acf(
        mut self,
        control_acf_path: Option<String>,
        control_reload_acf_path: Option<String>,
    ) -> Self {
        let spec = self
            .downstreams
            .last_mut()
            .expect("downstream_control_acf called before add_downstream");
        spec.control_acf_path = control_acf_path;
        spec.control_reload_acf_path = control_reload_acf_path;
        self
    }

    /// Set the access policy for the most-recently-added downstream
    /// (the one from the preceding [`Self::add_downstream`] call). Every
    /// upstream proxy of that downstream is wrapped in
    /// `Audit( ReadOnly?( Acl( source ) ) )` — the same chain the
    /// single-tenant [`super::PvaGateway`] applies — so migrating a
    /// deployment to the multi-tenant topology does not silently drop
    /// access control. The control/diagnostic source stays unwrapped
    /// (its PVs are read-only diagnostics).
    ///
    /// `acl: None` ⇒ permissive; `audit: None` ⇒ no audit. Panics if
    /// called before any [`Self::add_downstream`].
    pub fn downstream_access(
        mut self,
        acl: Option<AclConfig>,
        read_only: bool,
        audit: Option<Arc<dyn AuditSink>>,
    ) -> Self {
        let spec = self
            .downstreams
            .last_mut()
            .expect("downstream_access called before add_downstream");
        spec.acl = acl;
        spec.read_only = read_only;
        spec.audit = audit;
        self
    }

    pub fn cleanup_interval(mut self, d: Duration) -> Self {
        self.cleanup_interval = d;
        self
    }

    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }

    pub fn max_cache_entries(mut self, n: usize) -> Self {
        self.max_cache_entries = n;
        self
    }

    pub fn max_subscribers(mut self, n: usize) -> Self {
        self.max_subscribers = n;
        self
    }

    /// Validate + start every downstream. The returned
    /// [`MultiTenantPvaGateway`] owns one [`PvaServer`] per downstream
    /// spec and one [`ChannelCache`] per upstream tenant.
    pub fn start(self) -> GwResult<MultiTenantPvaGateway> {
        if self.upstreams.is_empty() {
            return Err(GwError::Other(
                "MultiTenantPvaGatewayBuilder: at least one upstream required".into(),
            ));
        }
        if self.downstreams.is_empty() {
            return Err(GwError::Other(
                "MultiTenantPvaGatewayBuilder: at least one downstream required \
                 (a gateway with no listeners would resolve no clients)"
                    .into(),
            ));
        }
        // Detect duplicate upstream labels — a server's label list is
        // matched against this set, so duplicates would silently
        // route to whichever entry came first.
        for (i, a) in self.upstreams.iter().enumerate() {
            for b in &self.upstreams[i + 1..] {
                if a.label == b.label {
                    return Err(GwError::Other(format!(
                        "duplicate upstream label '{}'",
                        a.label
                    )));
                }
            }
        }
        // Same check for downstreams. `downstream(label)` accessor
        // returns the FIRST match, so duplicate labels would silently
        // shadow the second one — better to refuse at build time.
        for (i, a) in self.downstreams.iter().enumerate() {
            for b in &self.downstreams[i + 1..] {
                if a.label == b.label {
                    return Err(GwError::Other(format!(
                        "duplicate downstream label '{}'",
                        a.label
                    )));
                }
            }
            if a.upstream_labels.is_empty() {
                return Err(GwError::Other(format!(
                    "downstream '{}' must reference at least one upstream",
                    a.label
                )));
            }
        }
        // Build a cache per upstream. Sized identically — the per-PV
        // entry is per-client so the budgets don't share.
        let mut caches: Vec<(String, Arc<ChannelCache>)> = Vec::with_capacity(self.upstreams.len());
        for u in &self.upstreams {
            let c = ChannelCache::with_max_entries(
                u.client.clone(),
                self.cleanup_interval,
                self.max_cache_entries,
            );
            caches.push((u.label.clone(), c));
        }

        let mut servers: Vec<DownstreamHandle> = Vec::with_capacity(self.downstreams.len());
        for ds in self.downstreams {
            // Resolve each label to a cache; refuse unknown labels at
            // build time so misconfigured deployments surface early.
            let mut sources: Vec<(String, Arc<ChannelCache>)> = Vec::new();
            for needed in &ds.upstream_labels {
                let cache = caches
                    .iter()
                    .find(|(lbl, _)| lbl == needed)
                    .map(|(_, c)| c.clone())
                    .ok_or_else(|| {
                        GwError::Other(format!(
                            "downstream '{}' references unknown upstream label '{needed}'",
                            ds.label
                        ))
                    })?;
                sources.push((needed.clone(), cache));
            }
            // Compose the ChannelSource: optional control source at
            // order=-100 (so its names always win), then one
            // GatewayChannelSource per upstream label in spec order.
            let composite = CompositeSource::new();
            // Track the first gateway source for the optional
            // ControlSource (its `liveSubscribers` counter is
            // per-source). When multiple upstreams are present, we
            // pick the first one for the control surface — the
            // operator can always disambiguate via the per-cache
            // diagnostic PVs in each control_prefix namespace.
            // this downstream's access policy wraps *every* one
            // of its upstream proxies. Resolve the defaults once
            // (permissive ACL / NoopAudit) and apply the same
            // `Audit( ReadOnly?( Acl( source ) ) )` chain the
            // single-tenant gateway applies, via the shared
            // `layer_access_control` owner of the chain shape — so a
            // multi-tenant deployment cannot silently lose access
            // control. The audit sink is shared (`Arc`) across the
            // downstream's proxies.
            let acl_cfg = ds.acl.clone().unwrap_or_default();
            let audit_sink: Arc<dyn AuditSink> =
                ds.audit.clone().unwrap_or_else(|| Arc::new(NoopAudit));
            // Collect EVERY per-tenant proxy source (unlayered) so the
            // optional control source can administer all of them, not
            // just the first. The control
            // source operates on the unlayered proxies — its diagnostic
            // PVs are not access-gated.
            let mut gw_sources: Vec<GatewayChannelSource> = Vec::new();
            for (i, (label, cache)) in sources.iter().enumerate() {
                let mut src = GatewayChannelSource::new(cache.clone());
                src.connect_timeout = self.connect_timeout;
                src.max_subscribers = self.max_subscribers;
                // Per-credential caches built lazily by this source must
                // honor the configured policy too.
                src.cleanup_interval = self.cleanup_interval;
                src.per_credential_max_entries = self.max_cache_entries;
                gw_sources.push(src.clone());
                let order = i as i32; // earlier labels win
                let name = format!("gateway:{label}");
                let layered =
                    layer_access_control(src, acl_cfg.clone(), ds.read_only, audit_sink.clone());
                composite.add_source(&name, layered, order).map_err(|e| {
                    GwError::Other(format!(
                        "downstream '{}' source '{name}' registration: {e}",
                        ds.label
                    ))
                })?;
            }
            if let Some(prefix) = ds.control_prefix.as_ref() {
                if !prefix.is_empty() {
                    if let Some((first, rest)) = gw_sources.split_first() {
                        let mut control = ControlSource::new(prefix, first.clone());
                        for src in rest {
                            control = control.with_source(src.clone());
                        }
                        // B6: gate the writable RPCs through the
                        // configured control ACF (closed when absent);
                        // wire the `:reload` default path. A bad path /
                        // unparseable file fails this downstream's
                        // build rather than silently closing the
                        // surface.
                        if let Some(path) = ds.control_acf_path.as_deref() {
                            control =
                                control.with_control_acf(super::gateway::load_acf_file(path)?);
                        }
                        if let Some(path) = ds.control_reload_acf_path.clone() {
                            control = control.with_acf_path(path);
                        }
                        composite
                            .add_source("__gw_control", Arc::new(control), -100)
                            .map_err(|e| {
                                GwError::Other(format!(
                                    "downstream '{}' control source registration: {e}",
                                    ds.label
                                ))
                            })?;
                    }
                }
            }
            let server = PvaServer::start(composite, ds.config)?;
            servers.push(DownstreamHandle {
                label: ds.label,
                server,
            });
        }

        Ok(MultiTenantPvaGateway { caches, servers })
    }
}

struct DownstreamHandle {
    label: String,
    server: PvaServer,
}

/// Running multi-tenant gateway. Drop to tear down all servers;
/// the per-upstream `ChannelCache`s drop alongside (their cleanup
/// task aborts via [`ChannelCache::drop`]).
pub struct MultiTenantPvaGateway {
    caches: Vec<(String, Arc<ChannelCache>)>,
    servers: Vec<DownstreamHandle>,
}

impl MultiTenantPvaGateway {
    /// Number of configured downstream servers.
    pub fn downstream_count(&self) -> usize {
        self.servers.len()
    }

    /// Number of configured upstream tenants.
    pub fn upstream_count(&self) -> usize {
        self.caches.len()
    }

    /// Look up a downstream by its label.
    pub fn downstream(&self, label: &str) -> Option<&PvaServer> {
        self.servers
            .iter()
            .find(|h| h.label == label)
            .map(|h| &h.server)
    }

    /// Look up an upstream cache by its label.
    pub fn upstream_cache(&self, label: &str) -> Option<&Arc<ChannelCache>> {
        self.caches
            .iter()
            .find(|(lbl, _)| lbl == label)
            .map(|(_, c)| c)
    }

    /// Stop every downstream server. Per-cache cleanup tasks are
    /// torn down when the gateway is dropped.
    pub fn stop_all(&self) {
        for h in &self.servers {
            h.server.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B6: an unreadable downstream control ACF must fail the
    /// multi-tenant build, not silently leave the writable surface
    /// closed — the same fail-fast contract the single-tenant gateway
    /// applies.
    #[tokio::test]
    async fn build_fails_when_downstream_control_acf_unreadable() {
        let client = Arc::new(PvaClient::builder().build());
        let res = MultiTenantPvaGatewayBuilder::new()
            .add_upstream("up", client)
            .add_downstream(
                "down",
                PvaServerConfig::isolated(),
                &["up"],
                Some("gw".to_string()),
            )
            .downstream_control_acf(Some("/no/such/pva_gw_control.acf".to_string()), None)
            .start();
        assert!(
            res.is_err(),
            "unreadable control ACF must fail the multi-tenant build"
        );
    }
}
