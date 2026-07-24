//! Internal command/control PVs.
//!
//! C ca-gateway publishes a set of writable "flag" PVs under the stats
//! prefix that let operators trigger command-file execution, reports,
//! an access/pvlist reload, and shutdown by `caput`-ing a positive value
//! — the cross-platform alternative to Unix SIGUSR1. The control PVs are
//! `statCommandFlag`, `statReport1Flag`, `statReport2Flag`,
//! `statReport3Flag`, `statNewAsFlag`, `statQuitFlag`, and
//! `statQuitServerFlag` (`gateServer.h:136-147`), published as
//! `<prefix>:<name>` by `initStats()` (`gateServer.cc:1877-2102`).
//!
//! A client write to one of these PVs invokes `gateStat::write()` →
//! `serv->processStat(type, val)` (`gateStat.cc:253-265`), which maps a
//! positive write to the matching control flag (`gateServer.cc:1838-1875`);
//! the main loop then executes the corresponding command and resets the
//! PV value to zero (`gateServer.cc:336-379`).
//!
//! This module mirrors that flag model exactly: each flag PV carries a
//! [`WriteHook`] that, on a positive write, RAISES a shared boolean flag in
//! [`ControlFlags`] (it does not enqueue a discrete command) and wakes the
//! single control owner. The owner ([`spawn_control_owner`]) drains the
//! raised flags once per pass in C's fixed main-loop order — `commandFlag`,
//! `report1Flag`, `report2Flag`, `report3Flag`, `newAsFlag`, `quitFlag`,
//! `quitServerFlag` (`gateServer.cc:336-379`) — dispatching each through the
//! same [`CommandHandler`] the SIGUSR1 path uses and resetting each consumed
//! flag PV back to zero. Multiple positive writes to one flag before a pass
//! therefore collapse to a single action, and a burst of different flags
//! runs in main-loop order, not client write order.

// RTEMS-EXEC-MODEL-ALLOW(7): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::pv::{WriteContext, WriteHook};
use epics_base_rs::types::EpicsValue;
use tokio::sync::Notify;

use super::command::{CommandHandler, GatewayCommand};

/// The control action a flag PV triggers when written with a positive
/// value. Mirrors C `gateServer::processStat`'s flag fan-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlTrigger {
    /// `commandFlag` → execute the configured command file (like SIGUSR1).
    CommandFile,
    /// `report1Flag` → full state report.
    Report1,
    /// `report2Flag` → summary report.
    Report2,
    /// `report3Flag` → access-security report.
    Report3,
    /// `newAsFlag` → reload access security + pvlist (C `newAs`).
    NewAs,
    /// `quitFlag` → graceful gateway shutdown.
    Quit,
    /// `quitServerFlag` → graceful gateway shutdown (server stop).
    QuitServer,
}

/// Shared control-flag state, owned by the single control task.
///
/// C ca-gateway does not enqueue a discrete command per write: a `caput` of
/// a positive value to a control PV raises a boolean server flag
/// (`gateStat::write` → `processStat`, `gateStat.cc:253-265` /
/// `gateServer.cc:1838-1875`). The gateway main loop later consumes those
/// booleans once per pass in a FIXED order — `commandFlag`, `report1Flag`,
/// `report2Flag`, `report3Flag`, `newAsFlag`, `quitFlag`, `quitServerFlag`
/// (`gateServer.cc:336-379`) — resetting each PV after its action. This
/// type is the Rust analogue: one [`AtomicBool`] per flag plus a [`Notify`]
/// to wake the owner. Because a write only sets a bool, repeated writes
/// before a pass collapse to one action, and the owner's fixed drain order
/// — not client write order — sequences a burst of different flags.
pub struct ControlFlags {
    /// Stats prefix with separator already applied; control PV names are
    /// `<prefix><suffix>` (the owner derives reset targets from this).
    prefix: String,
    command: AtomicBool,
    report1: AtomicBool,
    report2: AtomicBool,
    report3: AtomicBool,
    new_as: AtomicBool,
    quit: AtomicBool,
    quit_server: AtomicBool,
    /// Wakes the owner when any flag is raised. A `notify_one` permit is
    /// retained if the owner is not currently waiting, so no raise that
    /// lands mid-drain is lost — the owner re-drains on the next pass.
    wake: Notify,
}

impl ControlFlags {
    fn new(prefix: String) -> Self {
        Self {
            prefix,
            command: AtomicBool::new(false),
            report1: AtomicBool::new(false),
            report2: AtomicBool::new(false),
            report3: AtomicBool::new(false),
            new_as: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            quit_server: AtomicBool::new(false),
            wake: Notify::new(),
        }
    }

    /// Select the flag cell for a trigger.
    fn cell(&self, trigger: ControlTrigger) -> &AtomicBool {
        match trigger {
            ControlTrigger::CommandFile => &self.command,
            ControlTrigger::Report1 => &self.report1,
            ControlTrigger::Report2 => &self.report2,
            ControlTrigger::Report3 => &self.report3,
            ControlTrigger::NewAs => &self.new_as,
            ControlTrigger::Quit => &self.quit,
            ControlTrigger::QuitServer => &self.quit_server,
        }
    }

    /// Raise the flag for `trigger` and wake the owner. Called from the
    /// control PV write hook on a positive write. Idempotent within a pass:
    /// a second raise before the owner consumes the flag collapses to one
    /// action (C `setStat`/main-loop semantics).
    fn raise(&self, trigger: ControlTrigger) {
        self.cell(trigger).store(true, Ordering::Relaxed);
        self.wake.notify_one();
    }

    /// The full PV name for a control flag suffix under this prefix.
    fn pv_name(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.prefix)
    }
}

/// Flag PV suffix → trigger mapping. The suffixes match C's stat PV base
/// names (`gateServer.h:136-147`); published as `<prefix><suffix>`.
const CONTROL_FLAGS: &[(&str, ControlTrigger)] = &[
    ("commandFlag", ControlTrigger::CommandFile),
    ("report1Flag", ControlTrigger::Report1),
    ("report2Flag", ControlTrigger::Report2),
    ("report3Flag", ControlTrigger::Report3),
    ("newAsFlag", ControlTrigger::NewAs),
    ("quitFlag", ControlTrigger::Quit),
    ("quitServerFlag", ControlTrigger::QuitServer),
];

/// Whether a written value is "positive" — the C trigger condition
/// (`processStat` acts only on `val > 0`, `gateServer.cc:1838-1875`).
/// A zero or negative write (including the owner's own reset to 0) is a
/// no-op, so resetting the flag does not re-trigger.
fn is_positive(v: &EpicsValue) -> bool {
    match v {
        EpicsValue::Short(n) => *n > 0,
        EpicsValue::Long(n) => *n > 0,
        EpicsValue::Char(n) => *n > 0,
        EpicsValue::Enum(n) => *n > 0,
        EpicsValue::Int64(n) => *n > 0,
        EpicsValue::UInt64(n) => *n > 0,
        EpicsValue::Float(x) => *x > 0.0,
        EpicsValue::Double(x) => *x > 0.0,
        _ => false,
    }
}

/// Build the [`WriteHook`] for one control flag PV: on a positive write,
/// RAISE the trigger's shared flag and wake the owner. Reads/zero-writes
/// (including the owner's own reset to 0) are inert, so a reset never
/// re-raises.
fn control_write_hook(trigger: ControlTrigger, flags: Arc<ControlFlags>) -> WriteHook {
    Arc::new(move |value: EpicsValue, _ctx: WriteContext| {
        let trigger = trigger;
        let flags = flags.clone();
        Box::pin(async move {
            if is_positive(&value) {
                flags.raise(trigger);
            }
            Ok::<(), CaError>(())
        })
    })
}

/// Publish the C-compatible control flag PVs under `prefix`, each wired to
/// raise its shared flag on a positive write. No-op when `prefix` is empty
/// (stats disabled). Returns the shared [`ControlFlags`] the owner task
/// drains (and the hooks raise into).
///
/// Registered with a permissive (default) access decision, matching the
/// other Rust stat PVs; site ACLs can still gate them through the
/// `.pvlist`/ACF that governs every served name.
pub async fn publish_control_pvs(db: &PvDatabase, prefix: &str) -> Option<Arc<ControlFlags>> {
    if prefix.is_empty() {
        return None;
    }
    let flags = Arc::new(ControlFlags::new(prefix.to_string()));
    for (suffix, trigger) in CONTROL_FLAGS {
        let pv = format!("{prefix}{suffix}");
        let hook = control_write_hook(*trigger, flags.clone());
        // C ca-gateway creates the control flag PVs (statCommandFlag /
        // statReport1..3Flag / statNewAsFlag / statQuitFlag /
        // statQuitServerFlag) as plain `gateStat` (gateServer.cc:1768),
        // whose `bestExternalType()` is DBR_DOUBLE (gateStat.cc:27
        // `#define STAT_DOUBLE` → aitEnumFloat64, gateStat.cc:235-242) —
        // NOT the write-disabled string `gateStatDesc`. Register as Double
        // so the native type seen at CREATE_CHANNEL matches C (same
        // contract stats.rs:246-250 enforces for the gateStat aliases).
        if let Err(e) = db
            .add_pv_with_hooks(&pv, EpicsValue::Double(0.0), hook, None)
            .await
        {
            tracing::warn!(
                pv = %pv,
                error = %e,
                "ca_gateway control: pre-register skipped (name already in use)"
            );
        }
    }
    Some(flags)
}

/// Spawn the single control owner that drains the raised flags and
/// dispatches each through `handler` — the same `CommandHandler` the
/// SIGUSR1 path uses.
///
/// On each wake the owner drains the flags ONCE in C's fixed main-loop
/// order — `commandFlag`, `report1Flag`, `report2Flag`, `report3Flag`,
/// `newAsFlag`, then `quitFlag`/`quitServerFlag` (`gateServer.cc:336-379`).
/// Each `swap(false)` consumes-and-collapses the flag (duplicate writes
/// that arrived before this pass become one action), and after each action
/// the flag PV is reset to zero. `quit`/`quitServer` are checked LAST, so a
/// quit raised in the same pass as a report/reload never pre-empts those
/// earlier flags; once consumed it fires `shutdown` and the owner exits.
pub fn spawn_control_owner(
    flags: Arc<ControlFlags>,
    handler: CommandHandler,
    db: Arc<PvDatabase>,
    command_path: Option<std::path::PathBuf>,
    shutdown: Arc<Notify>,
) -> epics_base_rs::runtime::task::TaskHandle<()> {
    epics_base_rs::runtime::task::spawn(async move {
        loop {
            flags.wake.notified().await;

            // commandFlag → run the command file (which itself collapses to
            // C's four R1/R2/AS/R3 flags in fixed order, see command.rs).
            if flags.command.swap(false, Ordering::Relaxed) {
                run_command_file(&handler, &command_path).await;
                reset_flag_pv(&db, &flags.pv_name("commandFlag")).await;
            }
            if flags.report1.swap(false, Ordering::Relaxed) {
                dispatch_logged(&handler, GatewayCommand::ReportFull).await;
                reset_flag_pv(&db, &flags.pv_name("report1Flag")).await;
            }
            if flags.report2.swap(false, Ordering::Relaxed) {
                dispatch_logged(&handler, GatewayCommand::ReportSummary).await;
                reset_flag_pv(&db, &flags.pv_name("report2Flag")).await;
            }
            if flags.report3.swap(false, Ordering::Relaxed) {
                dispatch_logged(&handler, GatewayCommand::ReportAccess).await;
                reset_flag_pv(&db, &flags.pv_name("report3Flag")).await;
            }
            if flags.new_as.swap(false, Ordering::Relaxed) {
                dispatch_logged(&handler, GatewayCommand::ReloadAccess).await;
                reset_flag_pv(&db, &flags.pv_name("newAsFlag")).await;
            }

            // quit / quitServer come last in the C loop. Consume both flags
            // and reset their PVs before signaling shutdown, so they never
            // skip the report/reload flags handled above in the same pass.
            let quit = flags.quit.swap(false, Ordering::Relaxed);
            let quit_server = flags.quit_server.swap(false, Ordering::Relaxed);
            if quit {
                reset_flag_pv(&db, &flags.pv_name("quitFlag")).await;
            }
            if quit_server {
                reset_flag_pv(&db, &flags.pv_name("quitServerFlag")).await;
            }
            if quit || quit_server {
                tracing::info!(
                    quit,
                    quit_server,
                    "ca-gateway-rs: shutdown requested via control PV"
                );
                // The run loop performs the actual teardown.
                shutdown.notify_one();
                return;
            }
        }
    })
}

/// Run the configured command file (the `commandFlag` action), logging its
/// output. A missing command-file path is a warned no-op, matching the
/// previous behaviour.
async fn run_command_file(handler: &CommandHandler, command_path: &Option<std::path::PathBuf>) {
    match command_path {
        Some(path) => match handler.process_file(path).await {
            Ok(out) if !out.is_empty() => {
                tracing::info!(output = %out.trim_end(), "ca-gateway-rs: commandFlag output");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "ca-gateway-rs: commandFlag file error");
            }
        },
        None => {
            tracing::warn!("ca-gateway-rs: commandFlag written but no command file configured");
        }
    }
}

/// Reset a consumed control flag PV to zero. `Double(0.0)` matches the
/// DBR_DOUBLE native type the flag was registered with (gateStat parity);
/// the write hook treats a zero write as inert, so the reset never
/// re-raises the flag.
async fn reset_flag_pv(db: &Arc<PvDatabase>, pv: &str) {
    if let Err(e) = db.put_pv_and_post(pv, EpicsValue::Double(0.0)).await {
        tracing::debug!(pv = %pv, error = %e, "ca-gateway-rs: control flag reset failed");
    }
}

async fn dispatch_logged(handler: &CommandHandler, cmd: GatewayCommand) {
    match handler.dispatch(cmd).await {
        Ok(out) if !out.is_empty() => {
            tracing::info!(output = %out.trim_end(), "ca-gateway-rs: control report output");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "ca-gateway-rs: control command error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ca_gateway::access::AccessConfig;
    use crate::ca_gateway::cache::PvCache;
    use crate::ca_gateway::pvlist::PvList;
    use crate::ca_gateway::stats::Stats;
    use arc_swap::ArcSwap;
    use epics_base_rs::server::pv::ProcessVariable;
    use tokio::sync::RwLock;

    /// Invoke a control PV's installed write hook directly, the way the
    /// CA server's write path would (without a network round-trip).
    async fn fire(pv: &Arc<ProcessVariable>, v: EpicsValue) -> Result<(), CaError> {
        let hook = pv.write_hook().expect("control PV has a write hook");
        hook(v, WriteContext::default()).await
    }

    /// is_positive matches C's `val > 0` trigger condition across the
    /// numeric DBR types, and a zero/negative write (the owner's reset)
    /// is inert so it never re-triggers.
    #[test]
    fn positive_write_triggers_only_above_zero() {
        assert!(is_positive(&EpicsValue::Long(1)));
        assert!(is_positive(&EpicsValue::Double(0.5)));
        assert!(is_positive(&EpicsValue::Short(2)));
        assert!(!is_positive(&EpicsValue::Long(0)));
        assert!(!is_positive(&EpicsValue::Long(-1)));
        assert!(!is_positive(&EpicsValue::Double(0.0)));
        assert!(!is_positive(&EpicsValue::String("1".into())));
    }

    /// Every control flag PV is registered as DBR_DOUBLE, matching C
    /// ca-gateway which creates them as `gateStat` (gateServer.cc:1768 →
    /// STAT_DOUBLE/aitEnumFloat64). Mirrors
    /// `compat_alias_stats_are_double_native_type` (stats.rs). A downstream
    /// caget/dbpr must see DBR_DOUBLE, not the pre-fix DBR_LONG.
    #[tokio::test]
    async fn control_flags_are_double_native_type() {
        let db = PvDatabase::new();
        publish_control_pvs(&db, "gw:")
            .await
            .expect("non-empty prefix publishes control PVs");
        for (suffix, _) in CONTROL_FLAGS {
            let name = format!("gw:{suffix}");
            assert!(
                matches!(db.get_pv(&name), Ok(EpicsValue::Double(v)) if v == 0.0),
                "control flag {name} must register as DBR_DOUBLE (gateStat parity)"
            );
        }
    }

    /// publishing registers every control flag PV under the prefix, and a
    /// positive write RAISES exactly that flag in the shared `ControlFlags`
    /// (the hook→flag path). A zero write raises nothing.
    #[tokio::test]
    async fn control_pv_positive_write_raises_flag() {
        let db = PvDatabase::new();
        let flags = publish_control_pvs(&db, "gw:")
            .await
            .expect("non-empty prefix publishes control PVs");

        // Every flag PV is registered and readable (initial 0).
        for (suffix, _) in CONTROL_FLAGS {
            let name = format!("gw:{suffix}");
            assert!(
                db.find_pv(&name).await.is_some(),
                "control PV {name} must be registered"
            );
        }

        let pv = db.find_pv("gw:newAsFlag").await.unwrap();
        // A zero write is inert (so the owner's reset never re-raises).
        fire(&pv, EpicsValue::Double(0.0)).await.unwrap();
        assert!(
            !flags.new_as.load(Ordering::Relaxed),
            "a zero/reset write must not raise the flag"
        );
        // A positive write raises exactly the newAs flag.
        fire(&pv, EpicsValue::Long(1)).await.unwrap();
        assert!(
            flags.new_as.load(Ordering::Relaxed),
            "positive write raises newAs"
        );
        assert!(
            !flags.report2.load(Ordering::Relaxed),
            "a write to newAsFlag must not raise an unrelated flag"
        );
    }

    /// the owner dispatches a Report flag through the shared CommandHandler
    /// and resets the raising flag PV back to zero (DBR_DOUBLE 0.0).
    #[tokio::test]
    async fn owner_dispatches_and_resets_flag() {
        let db = Arc::new(PvDatabase::new());
        let flags = publish_control_pvs(&db, "gw:").await.unwrap();

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(cache, pvlist, access, None, None);
        let shutdown = Arc::new(Notify::new());
        let owner = spawn_control_owner(flags.clone(), handler, db.clone(), None, shutdown);

        // Drive report2Flag positive via its write hook.
        let pv = db.find_pv("gw:report2Flag").await.unwrap();
        fire(&pv, EpicsValue::Long(1)).await.unwrap();

        // The owner resets the flag back to 0 after handling. The flag is
        // a DBR_DOUBLE gateStat PV, so the reset value is Double(0.0).
        let mut reset = false;
        for _ in 0..50 {
            if matches!(
                db.get_pv("gw:report2Flag"),
                Ok(EpicsValue::Double(v)) if v == 0.0
            ) {
                reset = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(reset, "owner must reset the flag PV to zero after handling");
        owner.abort();
    }

    /// a quit flag fires the shutdown Notify and stops the owner.
    #[tokio::test]
    async fn quit_trigger_signals_shutdown() {
        let db = Arc::new(PvDatabase::new());
        let flags = publish_control_pvs(&db, "gw:").await.unwrap();

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(cache, pvlist, access, None, None);
        let shutdown = Arc::new(Notify::new());
        let owner = spawn_control_owner(flags.clone(), handler, db.clone(), None, shutdown.clone());

        let pv = db.find_pv("gw:quitFlag").await.unwrap();
        fire(&pv, EpicsValue::Long(1)).await.unwrap();

        // shutdown must be notified within a bounded wait.
        let notified = tokio::time::timeout(std::time::Duration::from_secs(2), shutdown.notified())
            .await
            .is_ok();
        assert!(notified, "quit flag must fire the shutdown Notify");

        // The owner task exits after a quit.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), owner).await;
    }

    /// Build a report-file-backed CommandHandler so dispatched reports are
    /// observable (each R1/R2/R3 appends a section, R2 = SIGUSR2 shortcut).
    fn report_handler(report_path: std::path::PathBuf) -> CommandHandler {
        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        CommandHandler::new(cache, pvlist, access, None, None)
            .with_stats(Arc::new(Stats::new("gw".to_string())))
            .with_report_path(Some(report_path))
    }

    /// Poll the report file until `needle` appears (bounded), then return
    /// the full body.
    async fn wait_for_report(path: &std::path::Path, needle: &str) -> String {
        for _ in 0..200 {
            if let Ok(body) = std::fs::read_to_string(path) {
                if body.contains(needle) {
                    return body;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Multiple positive writes to one flag before the owner's pass collapse
    /// to ONE action — C sets a boolean and consumes it once per main-loop
    /// pass (gateServer.cc:347-351). Three report2Flag raises must produce a
    /// single R2 report section, not three.
    #[tokio::test]
    async fn control_owner_collapses_duplicate_writes() {
        let pid = std::process::id();
        let report_path = std::env::temp_dir().join(format!("ca_gw_ctl_collapse_{pid}.report"));
        let _ = std::fs::remove_file(&report_path);

        let db = Arc::new(PvDatabase::new());
        let flags = publish_control_pvs(&db, "gw:").await.unwrap();
        // Raise report2 three times BEFORE the owner runs: the notify
        // permits coalesce and the flag stays a single bool, so the owner
        // drains it exactly once.
        flags.raise(ControlTrigger::Report2);
        flags.raise(ControlTrigger::Report2);
        flags.raise(ControlTrigger::Report2);

        let shutdown = Arc::new(Notify::new());
        let owner = spawn_control_owner(
            flags.clone(),
            report_handler(report_path.clone()),
            db.clone(),
            None,
            shutdown,
        );

        let body = wait_for_report(&report_path, "R2 (process variable report)").await;
        // Settle: any erroneous second pass would have to wake again, but no
        // further raise happened (the reset write is inert), so the count is
        // stable. A short extra wait guards against an in-flight second pass.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let body = std::fs::read_to_string(&report_path).unwrap_or(body);
        let count = body.matches("R2 (process variable report)").count();
        assert_eq!(
            count, 1,
            "three report2Flag writes must collapse to one report: {count} sections"
        );

        owner.abort();
        let _ = std::fs::remove_file(&report_path);
    }

    /// A burst of different flags runs in C's FIXED main-loop order, not
    /// client write order: report1/2/3 raised in reverse order still append
    /// R1, then R2, then R3 (gateServer.cc:342-356).
    #[tokio::test]
    async fn control_owner_runs_flags_in_fixed_c_order() {
        let pid = std::process::id();
        let report_path = std::env::temp_dir().join(format!("ca_gw_ctl_order_{pid}.report"));
        let _ = std::fs::remove_file(&report_path);

        let db = Arc::new(PvDatabase::new());
        let flags = publish_control_pvs(&db, "gw:").await.unwrap();
        // Raise in reverse C order to prove ordering is by the owner, not by
        // write order.
        flags.raise(ControlTrigger::Report3);
        flags.raise(ControlTrigger::Report2);
        flags.raise(ControlTrigger::Report1);

        let shutdown = Arc::new(Notify::new());
        let owner = spawn_control_owner(
            flags.clone(),
            report_handler(report_path.clone()),
            db.clone(),
            None,
            shutdown,
        );

        // R3 is dispatched last; once it is present all three are.
        let body = wait_for_report(&report_path, "R3 (access security report)").await;
        let r1 = body.find("R1 (PV report)").expect("R1 present");
        let r2 = body
            .find("R2 (process variable report)")
            .expect("R2 present");
        let r3 = body
            .find("R3 (access security report)")
            .expect("R3 present");
        assert!(
            r1 < r2 && r2 < r3,
            "fixed C order R1<R2<R3 — got R1={r1} R2={r2} R3={r3}"
        );

        owner.abort();
        let _ = std::fs::remove_file(&report_path);
    }

    /// A quit raised in the same pass as a report does NOT skip the earlier
    /// flag: C checks quitFlag/quitServerFlag last in the loop
    /// (gateServer.cc:363-378), so the report still runs before shutdown.
    #[tokio::test]
    async fn control_owner_quit_does_not_skip_earlier_flags() {
        let pid = std::process::id();
        let report_path = std::env::temp_dir().join(format!("ca_gw_ctl_quit_{pid}.report"));
        let _ = std::fs::remove_file(&report_path);

        let db = Arc::new(PvDatabase::new());
        let flags = publish_control_pvs(&db, "gw:").await.unwrap();
        // Quit raised together with (even "before", by C order) report2.
        flags.raise(ControlTrigger::Quit);
        flags.raise(ControlTrigger::Report2);

        let shutdown = Arc::new(Notify::new());
        let owner = spawn_control_owner(
            flags.clone(),
            report_handler(report_path.clone()),
            db.clone(),
            None,
            shutdown.clone(),
        );

        // The report ran despite the quit in the same pass.
        let body = wait_for_report(&report_path, "R2 (process variable report)").await;
        assert!(
            body.contains("R2 (process variable report)"),
            "report2 must run before quit in the same pass"
        );
        // And the quit still fired shutdown after the earlier flag.
        let notified = tokio::time::timeout(std::time::Duration::from_secs(2), shutdown.notified())
            .await
            .is_ok();
        assert!(notified, "quit must still fire shutdown after the report");

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), owner).await;
        let _ = std::fs::remove_file(&report_path);
    }
}
