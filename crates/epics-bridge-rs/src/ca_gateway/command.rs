//! Runtime command interface.
//!
//! Corresponds to C++ ca-gateway's `gateway.command` file + SIGUSR1
//! signal handler. The C++ gateway watches a command file: when SIGUSR1
//! arrives, it reads commands like `R1` (report), `R2` (summary),
//! `R3` (access report), `AS` (reload access security AND pvlist, like
//! C `gateServer::newAs`), `PVL` (reload pvlist only — a Rust extension).
//!
//! In Rust we offer two interfaces:
//!
//! 1. **Signal handler**: Unix-only. SIGUSR1 reads the command file and
//!    dispatches commands. Used in production deployments.
//! 2. **Programmatic**: [`CommandHandler::dispatch`] for direct invocation
//!    from tests or REST APIs.

// RTEMS-EXEC-MODEL-ALLOW(8): checked - these run and pass in the feature-ON suite.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use arc_swap::ArcSwap;
use tokio::sync::RwLock;

use crate::error::BridgeResult;

use super::access::AccessConfig;
use super::cache::PvCache;
use super::pvlist::{PvList, parse_pvlist_file};
use super::report::{self, PvReportEntry, StatsSnapshot};
use super::stats::Stats;
use super::upstream::UpstreamManager;

/// Commands that can be issued to a running gateway at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayCommand {
    /// Print full state report.
    ReportFull,
    /// Print summary statistics.
    ReportSummary,
    /// Print access security report.
    ReportAccess,
    /// Reload access security file (.access) AND the pvlist, evicting
    /// now-denied PVs — matches C `gateServer::newAs`'s
    /// `reInitialize(accessFile, listFile)`.
    ReloadAccess,
    /// Reload PV list (.pvlist) only. Rust extension; C has no such
    /// standalone command (its `AS` reloads both).
    ReloadPvList,
    /// Print version info.
    Version,
    /// No-op (for parser).
    Noop,
}

impl GatewayCommand {
    /// Parse a single command line. Returns `Noop` for blank/comment lines.
    /// Retained as the programmatic single-command API; the command-file
    /// path uses the per-token parser (`Self::parse_token`) so it can
    /// honor C's multi-command-per-line shape.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Some(Self::Noop);
        }
        Self::parse_token(line)
    }

    /// Parse one whitespace-delimited token into a command, or `None` if
    /// the token is not a recognized command keyword. Case-insensitive,
    /// mirroring C ca-gateway's per-token string comparisons after
    /// `strtok` (gateServer.cc:461-470).
    fn parse_token(tok: &str) -> Option<Self> {
        match tok.to_ascii_uppercase().as_str() {
            "R1" | "REPORT" | "REPORT_FULL" => Some(Self::ReportFull),
            "R2" | "REPORT_SUMMARY" | "SUMMARY" => Some(Self::ReportSummary),
            "R3" | "REPORT_ACCESS" => Some(Self::ReportAccess),
            "AS" | "RELOAD_ACCESS" => Some(Self::ReloadAccess),
            "PVL" | "RELOAD_PVLIST" => Some(Self::ReloadPvList),
            "VERSION" | "V" => Some(Self::Version),
            _ => None,
        }
    }
}

/// Handles runtime commands against a live gateway.
pub struct CommandHandler {
    cache: Arc<RwLock<PvCache>>,
    pvlist: Arc<ArcSwap<PvList>>,
    /// Live access-security config slot. SIGUSR1 `AS` (RELOAD_ACCESS)
    /// re-reads the file and `store`s the new `Arc<AccessConfig>` so
    /// every per-PV WriteHook picks up the new rules without restart.
    access: Arc<ArcSwap<AccessConfig>>,
    /// Upstream subscription manager. ReloadPvList walks this on a
    /// pvlist edit and unsubscribes any PVs that no longer match —
    /// without the unsubscribe step, removed entries leak shadow
    /// PVs and upstream channels until restart.
    upstream: Option<Arc<UpstreamManager>>,
    pvlist_path: Option<PathBuf>,
    access_path: Option<PathBuf>,
    /// Beacon-anomaly throttle. After a successful `AS`/`PVL` pvlist
    /// reload — which may newly admit PVs — `request()` is fired so CA
    /// clients re-search and discover the now-available names immediately.
    /// Mirrors C `gateServer::newAs` calling `generateBeaconAnomaly()`
    /// after `reInitialize` "because new PVs may have become available"
    /// (gateServer.cc:684-686). `None` in stat-only/test handlers.
    beacon_anomaly: Option<Arc<super::beacon::BeaconAnomaly>>,
    /// Stats counters, snapshotted into the R1/R2 report headers. `None`
    /// in test handlers that don't exercise reports.
    stats: Option<Arc<Stats>>,
    /// R1/R2/R3 report file (C `-report`). When set, the report commands
    /// append C-compatible sections here and return only a status line;
    /// when `None` they return the rendered section (log-only fallback).
    report_path: Option<PathBuf>,
}

impl CommandHandler {
    pub fn new(
        cache: Arc<RwLock<PvCache>>,
        pvlist: Arc<ArcSwap<PvList>>,
        access: Arc<ArcSwap<AccessConfig>>,
        pvlist_path: Option<PathBuf>,
        access_path: Option<PathBuf>,
    ) -> Self {
        Self {
            cache,
            pvlist,
            access,
            upstream: None,
            pvlist_path,
            access_path,
            beacon_anomaly: None,
            stats: None,
            report_path: None,
        }
    }

    /// Attach the upstream manager so ReloadPvList can prune
    /// subscriptions for removed PVs. Must be called before
    /// the handler is used; cache-stat commands work without it.
    pub fn with_upstream(mut self, upstream: Arc<UpstreamManager>) -> Self {
        self.upstream = Some(upstream);
        self
    }

    /// Attach the stats counters used in R1/R2 report headers.
    pub fn with_stats(mut self, stats: Arc<Stats>) -> Self {
        self.stats = Some(stats);
        self
    }

    /// Set the R1/R2/R3 report file. When present, report commands append
    /// C-compatible sections to it (C `-report`, gateServer.cc:689-979).
    pub fn with_report_path(mut self, report_path: Option<PathBuf>) -> Self {
        self.report_path = report_path;
        self
    }

    /// Attach the gateway's beacon-anomaly throttle so an `AS`/`PVL`
    /// reload announces newly-available PVs to downstream CA clients.
    pub fn with_beacon_anomaly(mut self, beacon: Arc<super::beacon::BeaconAnomaly>) -> Self {
        self.beacon_anomaly = Some(beacon);
        self
    }

    /// Dispatch a command, returning the formatted output to print.
    pub async fn dispatch(&self, cmd: GatewayCommand) -> BridgeResult<String> {
        match cmd {
            GatewayCommand::Noop => Ok(String::new()),
            GatewayCommand::Version => Ok(format!("ca-gateway-rs {}\n", env!("CARGO_PKG_VERSION"))),
            GatewayCommand::ReportFull => {
                // R1 = C report1(): stats block + one line per virtual
                // connection (gateServer.cc:689-734).
                let stats = self.stats_snapshot().await;
                let pvs = self.collect_pv_report().await;
                let section = report::render_r1(&stats, &pvs);
                self.emit_report("R1", section, format!("{} PVs", pvs.len()))
            }
            GatewayCommand::ReportSummary => {
                // R2 = C report2(): state-grouped PV inventory with AS
                // group/level (gateServer.cc:736-953). Also the SIGUSR2
                // shortcut.
                let stats = self.stats_snapshot().await;
                let pvs = self.collect_pv_report().await;
                let section = report::render_r2(&stats, &pvs);
                self.emit_report("R2", section, format!("{} PVs", pvs.len()))
            }
            GatewayCommand::ReportAccess => {
                // R3 = C report3()/gateAs::report(): the .pvlist
                // allowed/denied tables + evaluation order + rules-installed
                // flags + the parsed UAG/HAG/ASG/RULE dump
                // (gateServer.cc:955-979, gateAs.cc:760-828). Rendered from
                // the live parsed structures, not the raw .access file text.
                let access = self.access.load_full();
                let mode = access.mode_summary();
                let as_dump = access.dump_report();
                let acf_path = self.access_path.as_ref().map(|p| p.display().to_string());
                let pvlist = self.pvlist.load_full();
                let section =
                    report::render_r3(mode, acf_path.as_deref(), &pvlist, as_dump.as_deref());
                let rules = pvlist.entries.len();
                self.emit_report("R3", section, format!("{rules} pvlist rules"))
            }
            GatewayCommand::ReloadPvList => match self.reload_pvlist_and_prune().await? {
                Some((count, pruned)) => Ok(format!(
                    "Reloaded pvlist: {count} rules ({pruned} PVs pruned)\n"
                )),
                None => Ok("No pvlist path configured\n".to_string()),
            },
            GatewayCommand::ReloadAccess => {
                // C `gateServer::newAs` (gateServer.cc:580) calls
                // `as->reInitialize(accessFile, listFile)`, which reloads
                // BOTH the access file (gateAs::initialize) AND the
                // pvlist (gateAs::readPvList, gateAs.cc:678-719), then
                // walks the PV lists evicting any now-denied PV. The
                // single `AS` command must therefore reload both, not
                // just the ACF — the pvlist-only `PVL` command is a Rust
                // extension, not a substitute for this.
                let mut out = String::new();
                match &self.access_path {
                    Some(path) => {
                        // Re-parse, then `store` the new `Arc` into the
                        // ArcSwap. In-flight puts that already loaded the
                        // previous `Arc` continue with the old rules;
                        // later puts pick up the new ones. Reload is
                        // wait-free — no lock against the put-hot-path.
                        let new_cfg = AccessConfig::from_file(path)?;
                        self.access.store(Arc::new(new_cfg));
                        out.push_str(&format!("Reloaded access file: {}\n", path.display()));
                    }
                    None => out.push_str("No access path configured\n"),
                }
                // Reload the pvlist + evict now-denied PVs, matching C
                // reInitialize's second half.
                if let Some((count, pruned)) = self.reload_pvlist_and_prune().await? {
                    out.push_str(&format!(
                        "Reloaded pvlist: {count} rules ({pruned} PVs pruned)\n"
                    ));
                }
                Ok(out)
            }
        }
    }

    /// Append a rendered report section to the configured report file and
    /// return a status line; or, when no report file is configured, return
    /// the rendered section itself so the log/programmatic path still
    /// carries the content (C always writes the file — the log-only
    /// fallback is a Rust convenience for deployments without `-report`).
    fn emit_report(&self, tag: &str, section: String, summary: String) -> BridgeResult<String> {
        match &self.report_path {
            Some(path) => {
                report::append_report(path, &section)?;
                Ok(format!(
                    "{tag} report appended to {} ({summary})\n",
                    path.display()
                ))
            }
            None => Ok(section),
        }
    }

    /// Snapshot the stats counters for the R1/R2 report headers. Returns a
    /// zeroed snapshot when no stats handle is attached (test handlers).
    async fn stats_snapshot(&self) -> StatsSnapshot {
        match &self.stats {
            Some(s) => StatsSnapshot {
                prefix: s.prefix().to_string(),
                client_event_count: s.total_events.load(Ordering::Relaxed),
                post_event_count: s.post_event_count.load(Ordering::Relaxed),
                exist_test_count: s.exist_count.load(Ordering::Relaxed),
                put_count: s.put_count.load(Ordering::Relaxed),
                read_only_rejects: s.read_only_rejects.load(Ordering::Relaxed),
                loop_count: s.loop_count.load(Ordering::Relaxed),
                heartbeat: s.heartbeat.load(Ordering::Relaxed),
                connected_hosts: s.host_count().await,
            },
            None => StatsSnapshot {
                prefix: String::new(),
                client_event_count: 0,
                post_event_count: 0,
                exist_test_count: 0,
                put_count: 0,
                read_only_rejects: 0,
                loop_count: 0,
                heartbeat: 0,
                connected_hosts: 0,
            },
        }
    }

    /// Collect one [`PvReportEntry`] per cached PV, resolving each PV's AS
    /// group / level / alias target from the current pvlist match (the
    /// cache entry itself does not store ASG/ASL — it is a property of the
    /// matched rule, re-derived here as the report is built).
    async fn collect_pv_report(&self) -> Vec<PvReportEntry> {
        let pvlist = self.pvlist.load_full();
        let cache = self.cache.read().await;
        let mut out = Vec::with_capacity(cache.len());
        for name in cache.names() {
            let entry_arc = match cache.get(&name) {
                Some(e) => e,
                None => continue,
            };
            let entry = entry_arc.read().await;
            let (asg, asl, resolved_name) = match pvlist.match_name(&name) {
                Some(m) => (m.asg.clone(), m.effective_asl(), Some(m.resolved_name)),
                None => (None, 1, None),
            };
            out.push(PvReportEntry {
                name: entry.name.clone(),
                state: entry.state,
                subscribers: entry.subscriber_count(),
                events: entry.event_count,
                asg,
                asl,
                resolved_name,
            });
        }
        out
    }

    /// Reload the pvlist file and prune now-denied PVs. Returns
    /// `Some((rule_count, pruned))` after a reload, or `None` if no
    /// pvlist path is configured.
    ///
    /// Shared by the `PVL` command (pvlist-only reload, a Rust
    /// extension) and the `AS` command, which — like C
    /// `gateServer::newAs` → `gateAs::reInitialize(accessFile, listFile)`
    /// — reloads both the access file and the pvlist.
    async fn reload_pvlist_and_prune(&self) -> BridgeResult<Option<(usize, usize)>> {
        let path = match &self.pvlist_path {
            Some(p) => p,
            None => return Ok(None),
        };
        let mut new = parse_pvlist_file(path)?;
        new.resolve_hosts().await;
        let count = new.entries.len();
        let new_arc = Arc::new(new);
        self.pvlist.store(new_arc.clone());

        // Walk every cached PV against the new pvlist, matching C
        // `gateServer::newAs`'s two-part pv_list walk (gateServer.cc):
        //
        // - No longer admitted → prune. Without this, removed entries
        //   leak shadow PVs and upstream channels until process restart;
        //   mirrors `pv->death()` + `list->remove()` for each denied PV.
        // - Still admitted → re-resolve its ASG/ASL and swap the live
        //   per-PV ACL so the read/write hooks enforce the new group/level
        //   immediately, instead of keeping the identity captured at first
        //   subscription. Mirrors `newAs` reinstalling the freshly-resolved
        //   `gateAsEntry` on each still-allowed PV (gateServer.cc:603-630).
        //   Already-connected downstream clients are then re-notified once
        //   after the walk (see the `notify_downstream_access_change` call
        //   below) — C `gateChan::resetAsClient` posting an access-rights
        //   event, gateVc.cc:170-199.
        //
        // Match via the same host-less `match_name` the resolver-prune
        // uses so alias rewrites are honored and only global rules apply.
        let mut pruned: usize = 0;
        if let Some(upstream) = &self.upstream {
            let cached_names: Vec<String> = self.cache.read().await.names();
            let mut acl_changed = false;
            for name in cached_names {
                match new_arc.match_name(&name) {
                    None => {
                        upstream.unsubscribe(&name).await;
                        self.cache.write().await.remove(&name);
                        pruned += 1;
                    }
                    Some(m) => {
                        let asl = m.effective_asl();
                        if upstream.update_acl(&name, m.asg, asl) {
                            acl_changed = true;
                        }
                    }
                }
            }
            // One asComputeAllAsg-style recompute pass per reload: when any
            // still-admitted PV's ACL actually changed, re-push
            // CA_PROTO_ACCESS_RIGHTS to already-connected clients so they see
            // the new group/level immediately instead of keeping the rights
            // advertised at first subscription (C `gateServer::newAs` →
            // `gateChan::resetAsClient`, gateVc.cc:170-199). The downstream
            // side still filters out channels whose computed level is
            // unchanged, so a no-op reload emits zero frames.
            if acl_changed {
                upstream.notify_downstream_access_change();
            }
        }

        // Announce the reload: a pvlist edit can newly admit PVs, so emit
        // a beacon anomaly to make downstream CA clients re-search and
        // discover the now-available names immediately, rather than
        // waiting for the next periodic beacon. The BeaconAnomaly throttle
        // collapses repeated reloads within its inhibit window. Mirrors C
        // gateServer::newAs calling generateBeaconAnomaly() after the
        // reInitialize (gateServer.cc:684-686).
        if let Some(beacon) = &self.beacon_anomaly {
            beacon.request();
        }
        Ok(Some((count, pruned)))
    }

    /// Process a `gateway.command` file with C `gateServer::gateCommands`
    /// semantics (gateServer.cc:434-496).
    ///
    /// C does NOT dispatch each token as it reads. It strips an inline `#`
    /// comment from every line, `strtok`s on whitespace, and sets four
    /// booleans (`r1Flag`/`r2Flag`/`r3Flag`/`asFlag`) — recognizing exactly
    /// the four tokens `R1`, `R2`, `R3`, `AS` via case-sensitive `strcmp`
    /// (gateServer.cc:462-465). Any other token prints `Invalid command`
    /// (gateServer.cc:467). After the whole file is read, it runs at most
    /// ONE of each in a fixed order — `R1`, `R2`, `AS`, `R3`
    /// (gateServer.cc:476-493), with `R3` deliberately after `AS` so the
    /// access report reflects the just-reloaded rules.
    ///
    /// Three C-parity properties this gives a C-compatible command file:
    /// duplicate flags collapse to one action (`R2 R2` runs one summary);
    /// a burst of different flags is ordered by C's fixed sequence, not by
    /// token order in the file (`R3 AS` reloads access first, then reports);
    /// and the broader Rust programmatic vocabulary
    /// (`GatewayCommand::parse_token`: `PVL`, `VERSION`, long aliases,
    /// lowercase) is NOT honored here — those are reported as invalid, so
    /// the command-file path matches C exactly while the control-PV /
    /// programmatic APIs keep the extensions.
    pub async fn process_file(&self, path: &PathBuf) -> BridgeResult<String> {
        let content = std::fs::read_to_string(path)?;

        // Pass 1: collapse the file into C's four flags and report invalid
        // tokens. Case-sensitive exact match, mirroring C `strcmp`.
        let (mut r1, mut r2, mut r3, mut reload_as) = (false, false, false, false);
        for raw in content.lines() {
            let line = match raw.find('#') {
                Some(i) => &raw[..i],
                None => raw,
            };
            for tok in line.split_whitespace() {
                match tok {
                    "R1" => r1 = true,
                    "R2" => r2 = true,
                    "R3" => r3 = true,
                    "AS" => reload_as = true,
                    other => tracing::warn!(
                        command = %other,
                        "ca-gateway-rs: invalid command in command file (C gateCommands \
                         accepts only R1/R2/R3/AS)"
                    ),
                }
            }
        }

        // Pass 2: run at most one of each in C's fixed order R1, R2, AS, R3.
        let mut combined = String::new();
        if r1 {
            combined.push_str(&self.dispatch(GatewayCommand::ReportFull).await?);
        }
        if r2 {
            combined.push_str(&self.dispatch(GatewayCommand::ReportSummary).await?);
        }
        if reload_as {
            combined.push_str(&self.dispatch(GatewayCommand::ReloadAccess).await?);
        }
        if r3 {
            combined.push_str(&self.dispatch(GatewayCommand::ReportAccess).await?);
        }
        Ok(combined)
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::PvState;
    use super::*;

    #[test]
    fn parse_known_commands() {
        assert_eq!(
            GatewayCommand::parse("R1"),
            Some(GatewayCommand::ReportFull)
        );
        assert_eq!(
            GatewayCommand::parse("r2"),
            Some(GatewayCommand::ReportSummary)
        );
        assert_eq!(
            GatewayCommand::parse("REPORT_ACCESS"),
            Some(GatewayCommand::ReportAccess)
        );
        assert_eq!(
            GatewayCommand::parse("AS"),
            Some(GatewayCommand::ReloadAccess)
        );
        assert_eq!(
            GatewayCommand::parse("PVL"),
            Some(GatewayCommand::ReloadPvList)
        );
        assert_eq!(GatewayCommand::parse("v"), Some(GatewayCommand::Version));
    }

    #[test]
    fn parse_blank_and_comment() {
        assert_eq!(GatewayCommand::parse(""), Some(GatewayCommand::Noop));
        assert_eq!(GatewayCommand::parse("   "), Some(GatewayCommand::Noop));
        assert_eq!(
            GatewayCommand::parse("# comment"),
            Some(GatewayCommand::Noop)
        );
    }

    #[test]
    fn parse_unknown() {
        assert!(GatewayCommand::parse("BOGUS").is_none());
    }

    #[tokio::test]
    async fn dispatch_version() {
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(cache, pvlist, access, None, None);
        let out = handler.dispatch(GatewayCommand::Version).await.unwrap();
        assert!(out.contains("ca-gateway-rs"));
    }

    #[tokio::test]
    async fn dispatch_summary_empty_cache() {
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(cache, pvlist, access, None, None);
        // No report file configured → the R2 section is returned directly.
        let out = handler
            .dispatch(GatewayCommand::ReportSummary)
            .await
            .unwrap();
        assert!(out.contains("R2 (process variable report)"));
        assert!(out.contains("total PVs=0 connecting=0 dead=0 disconnect=0 inactive=0 active=0"));
    }

    /// R1/R2/R3 must APPEND C-compatible report sections to the configured
    /// report file (C report1/report2/report3 open `-report` in append
    /// mode, gateServer.cc:689-979), returning only a status line — not the
    /// terse in-memory string the pre-fix handler logged.
    #[tokio::test]
    async fn reports_append_c_sections_to_report_file() {
        let pid = std::process::id();
        let dir = std::env::temp_dir();
        let acf_path = dir.join(format!("ca_gw_101_acf_{pid}.access"));
        let report_path = dir.join(format!("ca_gw_101_report_{pid}.report"));
        let _ = std::fs::remove_file(&report_path);
        std::fs::write(&acf_path, "ASG(DEFAULT) {\n    RULE(1, READ)\n}\n").unwrap();

        // Two PVs in distinct states so the R1 virtual-connection list and
        // the R2 state-grouped inventory both have representative content.
        let cache = Arc::new(RwLock::new(PvCache::new()));
        {
            let mut c = cache.write().await;
            c.get_or_create("BEAM:CURRENT")
                .write()
                .await
                .set_state(PvState::Active);
            c.get_or_create("VAC:PRESSURE")
                .write()
                .await
                .set_state(PvState::Dead);
        }
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        // R3 renders the *parsed* access-security structures (the
        // UAG/HAG/ASG/RULE dump), not the raw ACF file text — so load the
        // ACF as parsed rules here.
        let access = Arc::new(ArcSwap::from_pointee(
            AccessConfig::from_file(&acf_path).unwrap(),
        ));
        let handler = CommandHandler::new(cache, pvlist, access, None, Some(acf_path.clone()))
            .with_stats(Arc::new(Stats::new("gw".to_string())))
            .with_report_path(Some(report_path.clone()));

        // R1: appends, status names the file + PV count.
        let r1 = handler.dispatch(GatewayCommand::ReportFull).await.unwrap();
        assert!(r1.contains("R1 report appended to"));
        assert!(r1.contains("(2 PVs)"));
        // R2 then R3 append to the SAME file.
        let r2 = handler
            .dispatch(GatewayCommand::ReportSummary)
            .await
            .unwrap();
        assert!(r2.contains("R2 report appended to"));
        let r3 = handler
            .dispatch(GatewayCommand::ReportAccess)
            .await
            .unwrap();
        assert!(r3.contains("R3 report appended to"));

        let body = std::fs::read_to_string(&report_path).unwrap();
        // R1 section: stats block + virtual connection lines.
        assert!(body.contains("R1 (PV report)"));
        assert!(body.contains("virtual connections (2):"));
        assert!(body.contains("BEAM:CURRENT state=Active"));
        // R2 section: state-grouped inventory.
        assert!(body.contains("R2 (process variable report)"));
        assert!(body.contains("total PVs=2 connecting=0 dead=1 disconnect=0 inactive=0 active=1"));
        assert!(body.contains("VAC:PRESSURE asg=DEFAULT level=1"));
        // R3 section: access report carrying the PARSED AS dump
        // (UAG/HAG/ASG/RULE), not the raw ACF file text. The parsed dump
        // emits `RULE(1,READ)` (no space) under `ASG(DEFAULT)`, distinct
        // from the file's `RULE(1, READ)`.
        assert!(body.contains("R3 (access security report)"));
        assert!(body.contains("access security rules are installed."));
        assert!(body.contains("--- access security dump ---"));
        assert!(body.contains("ASG(DEFAULT)"));
        assert!(body.contains("RULE(1,READ)"));
        // The old raw-file echo must be gone.
        assert!(!body.contains("RULE(1, READ)"));
        // All three sections appended to one file (append, not truncate).
        let r1_pos = body.find("R1 (PV report)").unwrap();
        let r2_pos = body.find("R2 (process variable report)").unwrap();
        let r3_pos = body.find("R3 (access security report)").unwrap();
        assert!(r1_pos < r2_pos && r2_pos < r3_pos);

        let _ = std::fs::remove_file(&acf_path);
        let _ = std::fs::remove_file(&report_path);
    }

    /// the `AS` command must reload BOTH the access file and the
    /// pvlist, matching C `gateServer::newAs` →
    /// `gateAs::reInitialize(accessFile, listFile)` (gateAs.cc:678-719).
    /// Pre-fix `AS` reloaded only the ACF, leaving pvlist reload to the
    /// nonstandard `PVL` command — so an operator issuing the standard
    /// C `AS` command never picked up pvlist edits.
    #[tokio::test]
    async fn dispatch_as_reloads_both_access_and_pvlist() {
        let pid = std::process::id();
        let dir = std::env::temp_dir();
        let acf_path = dir.join(format!("ca_gw_a97_acf_{pid}.access"));
        let pvl_path = dir.join(format!("ca_gw_a97_pvl_{pid}.pvlist"));
        // ACF content proven to parse (see upstream.rs ACF tests).
        std::fs::write(
            &acf_path,
            "ASG(DEFAULT) {\n    RULE(0, READ) { UAG(ops) }\n}\n",
        )
        .unwrap();
        std::fs::write(&pvl_path, "Beam.*  ALLOW\ntest.*  DENY\n").unwrap();

        // Start with an empty pvlist so the reload is observable.
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(
            cache,
            pvlist.clone(),
            access,
            Some(pvl_path.clone()),
            Some(acf_path.clone()),
        );
        assert_eq!(pvlist.load().entries.len(), 0, "starts empty");

        let out = handler
            .dispatch(GatewayCommand::ReloadAccess)
            .await
            .unwrap();

        assert!(
            out.contains("Reloaded access file"),
            "AS must reload the access file: {out:?}"
        );
        assert!(
            out.contains("Reloaded pvlist"),
            "AS must ALSO reload the pvlist (C reInitialize): {out:?}"
        );
        assert_eq!(
            pvlist.load().entries.len(),
            2,
            "AS must apply the new pvlist rules"
        );

        let _ = std::fs::remove_file(&acf_path);
        let _ = std::fs::remove_file(&pvl_path);
    }

    /// a successful `PVL`/`AS` pvlist reload must fire a beacon anomaly so
    /// downstream CA clients re-search and discover newly-admitted PVs
    /// (C gateServer::newAs → generateBeaconAnomaly, gateServer.cc:684-686).
    /// Pre-fix the reload path never touched the beacon throttle.
    #[tokio::test]
    async fn reload_fires_beacon_anomaly() {
        let pid = std::process::id();
        let pvl_path = std::env::temp_dir().join(format!("ca_gw_a11_pvl_{pid}.pvlist"));
        std::fs::write(&pvl_path, "Beam.*  ALLOW\n").unwrap();

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let beacon = Arc::new(super::super::beacon::BeaconAnomaly::new());
        let handler = CommandHandler::new(cache, pvlist, access, Some(pvl_path.clone()), None)
            .with_beacon_anomaly(beacon.clone());

        // No request honored yet.
        assert!(beacon.elapsed().is_none(), "beacon untouched before reload");

        handler
            .dispatch(GatewayCommand::ReloadPvList)
            .await
            .unwrap();

        // The reload must have honored a beacon request (fresh throttle,
        // so the first request is always honored).
        assert!(
            beacon.elapsed().is_some(),
            "PVL reload must fire a beacon anomaly"
        );

        let _ = std::fs::remove_file(&pvl_path);
    }

    /// C `gateCommands` collapses multi-token/comment lines into the four
    /// flags and runs each once (gateServer.cc:458-493). A command file may
    /// put several tokens on one line and end in a trailing `#` comment, but
    /// duplicate flags collapse to ONE action and non-C tokens (`V`, `PVL`,
    /// long aliases, lowercase) are invalid — NOT dispatched.
    #[tokio::test]
    async fn process_file_collapses_duplicates_and_rejects_non_c_tokens() {
        let pid = std::process::id();
        let cmd_path = std::env::temp_dir().join(format!("ca_gw_a10_cmd_{pid}.command"));
        // Line 1: a non-C token (V) + R2 + trailing inline comment.
        // Line 2: a bare comment line (ignored).
        // Line 3: a duplicate R2 with an inline comment stripped.
        std::fs::write(&cmd_path, "V R2 # reload now\n# just a comment\nR2 #x\n").unwrap();

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(cache, pvlist, access, None, None);

        let out = handler.process_file(&cmd_path).await.unwrap();

        // `V` is not a C command-file token → no version line (C prints
        // "Invalid command V" and runs nothing for it).
        assert!(
            !out.contains("ca-gateway-rs 0") && !out.contains("ca-gateway-rs v"),
            "V must be an invalid command-file token, not a version dispatch: {out:?}"
        );
        // The two R2 tokens collapse to exactly ONE R2 section (C runs
        // report2 once regardless of how many R2 flags were set).
        let summaries = out.matches("R2 (process variable report)").count();
        assert_eq!(
            summaries, 1,
            "duplicate R2 tokens must collapse to one summary: {out:?}"
        );

        let _ = std::fs::remove_file(&cmd_path);
    }

    /// C `gateCommands` runs the flags in a FIXED order — `R1`, `R2`, `AS`,
    /// `R3` — independent of token order in the file, with `R3` after `AS`
    /// so the access report reflects the just-reloaded rules
    /// (gateServer.cc:476-493). A file written `R3 AS R1` must still emit
    /// R1, then R2-absent, then the AS reload, then R3.
    #[tokio::test]
    async fn process_file_runs_flags_in_fixed_c_order() {
        let pid = std::process::id();
        let cmd_path = std::env::temp_dir().join(format!("ca_gw_a10_order_{pid}.command"));
        // Tokens deliberately out of C order on the line.
        std::fs::write(&cmd_path, "R3 AS R1\n").unwrap();

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        // No access/pvlist path → AS emits its "No access path configured"
        // marker; that marker must appear BETWEEN R1 and R3.
        let handler = CommandHandler::new(cache, pvlist, access, None, None);

        let out = handler.process_file(&cmd_path).await.unwrap();

        let r1_pos = out
            .find("R1 (PV report)")
            .expect("R1 section must be present");
        let as_pos = out
            .find("No access path configured")
            .expect("AS reload marker must be present");
        let r3_pos = out
            .find("R3 (access security report)")
            .expect("R3 section must be present");
        assert!(
            r1_pos < as_pos && as_pos < r3_pos,
            "fixed C order is R1, (R2), AS, R3 — got positions R1={r1_pos} AS={as_pos} R3={r3_pos}: {out:?}"
        );
        // R2 was not requested, so no summary section.
        assert!(
            !out.contains("R2 (process variable report)"),
            "R2 was not in the file and must not run: {out:?}"
        );

        let _ = std::fs::remove_file(&cmd_path);
    }

    /// a reload with no beacon attached (stat-only/test handler) must not
    /// panic — the beacon call is guarded by the `Option`.
    #[tokio::test]
    async fn reload_without_beacon_is_noop() {
        let pid = std::process::id();
        let pvl_path = std::env::temp_dir().join(format!("ca_gw_a11_nob_{pid}.pvlist"));
        std::fs::write(&pvl_path, "Beam.*  ALLOW\n").unwrap();

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(cache, pvlist, access, Some(pvl_path.clone()), None);

        handler
            .dispatch(GatewayCommand::ReloadPvList)
            .await
            .expect("reload without a beacon handle must succeed");

        let _ = std::fs::remove_file(&pvl_path);
    }
}
