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

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::RwLock;

use crate::error::BridgeResult;

use super::access::AccessConfig;
use super::cache::PvCache;
use super::pvlist::{PvList, parse_pvlist_file};
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
    /// path uses the per-token parser ([`Self::parse_token`]) so it can
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
        }
    }

    /// Attach the upstream manager so ReloadPvList can prune
    /// subscriptions for removed PVs. Must be called before
    /// the handler is used; cache-stat commands work without it.
    pub fn with_upstream(mut self, upstream: Arc<UpstreamManager>) -> Self {
        self.upstream = Some(upstream);
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
            GatewayCommand::ReportSummary => {
                let cache = self.cache.read().await;
                Ok(format!("Summary: {} PVs in cache\n", cache.len()))
            }
            GatewayCommand::ReportFull => {
                let cache = self.cache.read().await;
                let mut out = format!("Full report ({} PVs):\n", cache.len());
                for name in cache.names() {
                    if let Some(entry_arc) = cache.get(&name) {
                        let entry = entry_arc.read().await;
                        out.push_str(&format!(
                            "  {} state={:?} subs={} events={}\n",
                            entry.name,
                            entry.state,
                            entry.subscriber_count(),
                            entry.event_count
                        ));
                    }
                }
                Ok(out)
            }
            GatewayCommand::ReportAccess => {
                let pvlist = self.pvlist.load_full();
                Ok(format!(
                    "Access report: {} pvlist rules, order={:?}\n",
                    pvlist.entries.len(),
                    pvlist.order
                ))
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
        //   Runtime re-notification of already-connected downstream
        //   clients (C `gateChan::resetAsClient` posting an access-rights
        //   event, gateVc.cc:170-199) needs a cross-crate
        //   `CaServer::notify_access_change()` and is deferred; new
        //   connections already re-evaluate the access hook.
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

    /// Process all commands from a command file.
    ///
    /// C ca-gateway strips an inline `#` comment from each line, then
    /// `strtok`s it on whitespace and dispatches EVERY recognized command
    /// token (gateServer.cc:458-470, :475-493) — so a single line may
    /// carry several commands (`R1 AS`) and may end in a trailing comment
    /// (`R1 # reload`). Parsing each whole line as one exact command (the
    /// old behavior) silently dropped those C-compatible shapes, making a
    /// `kill -USR1` appear to succeed while the intended command never
    /// ran. Tokenize like C: split on whitespace and dispatch each
    /// recognized token; unrecognized tokens are ignored, as in C.
    pub async fn process_file(&self, path: &PathBuf) -> BridgeResult<String> {
        let content = std::fs::read_to_string(path)?;
        let mut combined = String::new();
        for raw in content.lines() {
            // Strip an inline comment, then dispatch each token.
            let line = match raw.find('#') {
                Some(i) => &raw[..i],
                None => raw,
            };
            for tok in line.split_whitespace() {
                if let Some(cmd) = GatewayCommand::parse_token(tok) {
                    combined.push_str(&self.dispatch(cmd).await?);
                }
            }
        }
        Ok(combined)
    }
}

#[cfg(test)]
mod tests {
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
        let out = handler
            .dispatch(GatewayCommand::ReportSummary)
            .await
            .unwrap();
        assert!(out.contains("0 PVs"));
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

    /// C-compatible command files put several commands on one line and
    /// allow trailing `#` comments (gateServer.cc:458-470). process_file
    /// must tokenize and dispatch every recognized token, not parse each
    /// whole line as one exact command. Pre-fix `V R1 # note` was a no-op.
    #[tokio::test]
    async fn process_file_tokenizes_multi_command_and_comment_lines() {
        let pid = std::process::id();
        let cmd_path = std::env::temp_dir().join(format!("ca_gw_a10_cmd_{pid}.command"));
        // Line 1: two commands on one line + trailing inline comment.
        // Line 2: a bare comment line (ignored).
        // Line 3: a command with a leading-token comment stripped.
        std::fs::write(&cmd_path, "V R2 # reload now\n# just a comment\nR2 #x\n").unwrap();

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(cache, pvlist, access, None, None);

        let out = handler.process_file(&cmd_path).await.unwrap();

        // `V` produced a version line; both `R2` tokens produced a summary.
        assert!(out.contains("ca-gateway-rs"), "V token dispatched: {out:?}");
        let summaries = out.matches("Summary:").count();
        assert_eq!(
            summaries, 2,
            "both R2 tokens (line 1 and line 3) must dispatch: {out:?}"
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
