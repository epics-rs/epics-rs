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
//! This module mirrors that: each flag PV carries a [`WriteHook`] that,
//! on a positive write, enqueues a [`ControlEvent`] to the single command
//! owner. The owner ([`spawn_control_owner`]) dispatches it through the
//! same [`CommandHandler`] the SIGUSR1 path uses — one command owner for
//! every trigger source — and resets the flag PV back to zero.

use std::sync::Arc;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::pv::{WriteContext, WriteHook};
use epics_base_rs::types::EpicsValue;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

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

/// One control request: the action plus the flag PV that raised it (so
/// the owner can reset that PV to zero after handling, matching C).
#[derive(Debug, Clone)]
pub struct ControlEvent {
    pub trigger: ControlTrigger,
    pub pv: String,
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
/// enqueue the trigger to the command owner. Reads/zero-writes are inert.
fn control_write_hook(
    trigger: ControlTrigger,
    pv: String,
    tx: UnboundedSender<ControlEvent>,
) -> WriteHook {
    Arc::new(move |value: EpicsValue, _ctx: WriteContext| {
        let trigger = trigger;
        let pv = pv.clone();
        let tx = tx.clone();
        Box::pin(async move {
            if is_positive(&value) {
                // Best-effort: if the owner task is gone the gateway is
                // already shutting down, so a dropped trigger is benign.
                let _ = tx.send(ControlEvent { trigger, pv });
            }
            Ok::<(), CaError>(())
        })
    })
}

/// Publish the C-compatible control flag PVs under `prefix`, each wired to
/// send its trigger on a positive write. No-op when `prefix` is empty
/// (stats disabled). Returns the receiver the owner task drains.
///
/// Registered with a permissive (default) access decision, matching the
/// other Rust stat PVs; site ACLs can still gate them through the
/// `.pvlist`/ACF that governs every served name.
pub async fn publish_control_pvs(
    db: &PvDatabase,
    prefix: &str,
) -> Option<UnboundedReceiver<ControlEvent>> {
    if prefix.is_empty() {
        return None;
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    for (suffix, trigger) in CONTROL_FLAGS {
        let pv = format!("{prefix}{suffix}");
        let hook = control_write_hook(*trigger, pv.clone(), tx.clone());
        if let Err(e) = db
            .add_pv_with_hooks(&pv, EpicsValue::Long(0), hook, None)
            .await
        {
            tracing::warn!(
                pv = %pv,
                error = %e,
                "ca_gateway control: pre-register skipped (name already in use)"
            );
        }
    }
    Some(rx)
}

/// Spawn the single command owner that drains control-PV triggers and
/// dispatches each through `handler` — the same `CommandHandler` the
/// SIGUSR1 path uses. After handling, the raising flag PV is reset to
/// zero (C resets the value after consuming the flag,
/// `gateServer.cc:336-379`). `Quit`/`QuitServer` fire `shutdown` so the
/// run loop tears down gracefully, then the owner exits.
pub fn spawn_control_owner(
    mut rx: UnboundedReceiver<ControlEvent>,
    handler: CommandHandler,
    db: Arc<PvDatabase>,
    command_path: Option<std::path::PathBuf>,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev.trigger {
                ControlTrigger::CommandFile => {
                    if let Some(path) = &command_path {
                        match handler.process_file(path).await {
                            Ok(out) if !out.is_empty() => {
                                tracing::info!(output = %out.trim_end(), "ca-gateway-rs: commandFlag output");
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, "ca-gateway-rs: commandFlag file error");
                            }
                        }
                    } else {
                        tracing::warn!(
                            "ca-gateway-rs: commandFlag written but no command file configured"
                        );
                    }
                }
                ControlTrigger::Report1 => {
                    dispatch_logged(&handler, GatewayCommand::ReportFull).await
                }
                ControlTrigger::Report2 => {
                    dispatch_logged(&handler, GatewayCommand::ReportSummary).await
                }
                ControlTrigger::Report3 => {
                    dispatch_logged(&handler, GatewayCommand::ReportAccess).await
                }
                ControlTrigger::NewAs => {
                    dispatch_logged(&handler, GatewayCommand::ReloadAccess).await
                }
                ControlTrigger::Quit | ControlTrigger::QuitServer => {
                    tracing::info!(
                        trigger = ?ev.trigger,
                        "ca-gateway-rs: shutdown requested via control PV"
                    );
                    // Reset the flag, then signal shutdown and stop the
                    // owner — the run loop performs the actual teardown.
                    let _ = db.put_pv_and_post(&ev.pv, EpicsValue::Long(0)).await;
                    shutdown.notify_one();
                    return;
                }
            }
            // Reset the flag PV to zero after handling so a later write
            // re-triggers (C resets value to 0 in the main loop).
            if let Err(e) = db.put_pv_and_post(&ev.pv, EpicsValue::Long(0)).await {
                tracing::debug!(pv = %ev.pv, error = %e, "ca-gateway-rs: control flag reset failed");
            }
        }
    })
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

    /// publishing registers every control flag PV under the prefix, and a
    /// positive write to one enqueues exactly that flag's trigger naming
    /// the PV to reset. A zero write enqueues nothing.
    #[tokio::test]
    async fn control_pv_positive_write_enqueues_trigger() {
        let db = PvDatabase::new();
        let mut rx = publish_control_pvs(&db, "gw:")
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

        // A positive write to newAsFlag enqueues NewAs naming that PV.
        let pv = db.find_pv("gw:newAsFlag").await.unwrap();
        fire(&pv, EpicsValue::Long(1)).await.unwrap();

        let ev = rx.try_recv().expect("positive write enqueues a trigger");
        assert_eq!(ev.trigger, ControlTrigger::NewAs);
        assert_eq!(ev.pv, "gw:newAsFlag");

        // A zero write to the same PV enqueues nothing (so the owner's
        // reset-to-zero never re-triggers).
        fire(&pv, EpicsValue::Long(0)).await.unwrap();
        assert!(
            rx.try_recv().is_err(),
            "a zero/reset write must not enqueue a trigger"
        );
    }

    /// the owner dispatches a Report trigger through the shared
    /// CommandHandler and resets the raising flag PV back to zero.
    #[tokio::test]
    async fn owner_dispatches_and_resets_flag() {
        let db = Arc::new(PvDatabase::new());
        let rx = publish_control_pvs(&db, "gw:").await.unwrap();

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(cache, pvlist, access, None, None);
        let shutdown = Arc::new(Notify::new());
        let owner = spawn_control_owner(rx, handler, db.clone(), None, shutdown);

        // Drive report2Flag positive via its write hook.
        let pv = db.find_pv("gw:report2Flag").await.unwrap();
        fire(&pv, EpicsValue::Long(1)).await.unwrap();

        // The owner resets the flag back to 0 after handling.
        let mut reset = false;
        for _ in 0..50 {
            if let Ok(EpicsValue::Long(0)) = db.get_pv("gw:report2Flag").await {
                reset = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(reset, "owner must reset the flag PV to zero after handling");
        owner.abort();
    }

    /// a quit trigger fires the shutdown Notify and stops the owner.
    #[tokio::test]
    async fn quit_trigger_signals_shutdown() {
        let db = Arc::new(PvDatabase::new());
        let rx = publish_control_pvs(&db, "gw:").await.unwrap();

        let cache = Arc::new(RwLock::new(PvCache::new()));
        let pvlist = Arc::new(ArcSwap::from_pointee(PvList::new()));
        let access = Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all()));
        let handler = CommandHandler::new(cache, pvlist, access, None, None);
        let shutdown = Arc::new(Notify::new());
        let owner = spawn_control_owner(rx, handler, db.clone(), None, shutdown.clone());

        let pv = db.find_pv("gw:quitFlag").await.unwrap();
        fire(&pv, EpicsValue::Long(1)).await.unwrap();

        // shutdown must be notified within a bounded wait.
        let notified = tokio::time::timeout(std::time::Duration::from_secs(2), shutdown.notified())
            .await
            .is_ok();
        assert!(notified, "quit trigger must fire the shutdown Notify");

        // The owner task exits after a quit.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), owner).await;
    }
}
