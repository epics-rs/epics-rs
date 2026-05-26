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
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Some(Self::Noop);
        }
        match line.to_ascii_uppercase().as_str() {
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
    /// PVs and upstream channels until restart (B-G12).
    upstream: Option<Arc<UpstreamManager>>,
    pvlist_path: Option<PathBuf>,
    access_path: Option<PathBuf>,
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
        }
    }

    /// Attach the upstream manager so ReloadPvList can prune
    /// subscriptions for removed PVs (B-G12). Must be called before
    /// the handler is used; cache-stat commands work without it.
    pub fn with_upstream(mut self, upstream: Arc<UpstreamManager>) -> Self {
        self.upstream = Some(upstream);
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

    /// Reload the pvlist file and prune now-denied PVs (B-G12). Returns
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

        // Prune subscriptions for PVs that the new pvlist no longer
        // admits. Without this, removed entries leak shadow PVs and
        // upstream channels until process restart. Match against the new
        // pvlist via the same `match_name` the resolver uses so alias
        // rewrites are honored. Mirrors C `newAs`'s pv_list walk that
        // calls `pv->death()` + `list->remove()` for each denied PV.
        let mut pruned: usize = 0;
        if let Some(upstream) = &self.upstream {
            let cached_names: Vec<String> = self.cache.read().await.names();
            for name in cached_names {
                if new_arc.match_name(&name).is_none() {
                    upstream.unsubscribe(&name).await;
                    self.cache.write().await.remove(&name);
                    pruned += 1;
                }
            }
        }
        Ok(Some((count, pruned)))
    }

    /// Process all commands from a command file (one command per line).
    pub async fn process_file(&self, path: &PathBuf) -> BridgeResult<String> {
        let content = std::fs::read_to_string(path)?;
        let mut combined = String::new();
        for line in content.lines() {
            if let Some(cmd) = GatewayCommand::parse(line) {
                combined.push_str(&self.dispatch(cmd).await?);
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

    /// A9-7: the `AS` command must reload BOTH the access file and the
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
}
