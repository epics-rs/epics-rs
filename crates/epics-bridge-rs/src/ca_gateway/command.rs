//! Runtime command interface.
//!
//! Corresponds to C++ ca-gateway's `gateway.command` file + SIGUSR1
//! signal handler. The C++ gateway watches a command file: when SIGUSR1
//! arrives, it reads commands like `R1` (report), `R2` (summary),
//! `R3` (access report), `AS` (reload access), `PVL` (reload pvlist).
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
    /// Reload access security file (.access).
    ReloadAccess,
    /// Reload PV list (.pvlist).
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
            GatewayCommand::ReloadPvList => {
                let path = match &self.pvlist_path {
                    Some(p) => p,
                    None => return Ok("No pvlist path configured\n".to_string()),
                };
                let mut new = parse_pvlist_file(path)?;
                new.resolve_hosts().await;
                let count = new.entries.len();
                let new_arc = Arc::new(new);
                self.pvlist.store(new_arc.clone());

                // B-G12: prune subscriptions for PVs that the new
                // pvlist no longer admits. Without this, removed
                // entries leak shadow PVs and upstream channels
                // until process restart. Match against the new
                // pvlist via the same `match_name` the resolver
                // uses so alias rewrites are honored.
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
                Ok(format!(
                    "Reloaded pvlist: {count} rules ({pruned} PVs pruned)\n"
                ))
            }
            GatewayCommand::ReloadAccess => {
                let path = match &self.access_path {
                    Some(p) => p,
                    None => return Ok("No access path configured\n".to_string()),
                };
                // Re-parse, then `store` the new `Arc` into the
                // ArcSwap. In-flight puts that already loaded the
                // previous `Arc` continue with the old rules; later
                // puts pick up the new ones. Reload is wait-free —
                // no lock against the put-hot-path.
                let new_cfg = AccessConfig::from_file(path)?;
                self.access.store(Arc::new(new_cfg));
                Ok(format!("Reloaded access file: {}\n", path.display()))
            }
        }
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
}
