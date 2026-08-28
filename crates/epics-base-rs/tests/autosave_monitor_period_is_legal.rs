// RTEMS-EXEC-MODEL-ALLOW(2): sync tests that hand-build their own tokio runtime; run and pass in the exec-backend suite.
//! `create_monitor_set`'s period argument has to survive both ends of
//! the `i64` the shell hands it.
//!
//! Zero reached `Duration::from_secs(0)` and then tokio's `interval`,
//! whose constructor asserts a non-zero period, so the save task panicked
//! on the async backend; on the blocking backend nothing asserts and the
//! set rewrote its `.sav` in a tight loop. A negative value was cast
//! `as u32` and became 4294967295 seconds, so the set never saved again
//! while `fdblist` reported it idle.
//!
//! Both `into_builder` loops now read one already-legal `Duration`
//! produced by `MonitorSetDef::poll_period`, so the monitor path cannot
//! drift from the triggered path the way it had.
//!
//! No C reference: synApps `save_restore.c` is not present on this
//! machine, so these pin the port's own stated invariant.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::manager::AutosaveManager;
use epics_base_rs::server::autosave::save_set::SaveStrategy;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext, CommandDef};

fn make_ctx(rt: &tokio::runtime::Runtime) -> CommandContext {
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    CommandContext::new(Arc::new(PvDatabase::new()), bridge)
}

fn find<'a>(cmds: &'a [CommandDef], name: &str) -> &'a CommandDef {
    cmds.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not registered as an iocsh command"))
}

/// A startup config wired to a directory holding a loadable request
/// file — a set that cannot load its members no longer builds at all.
fn holder_with_req(dir: &tempfile::TempDir) -> Arc<Mutex<AutosaveStartupConfig>> {
    std::fs::write(dir.path().join("settings.req"), "IOC:setpoint\n").unwrap();
    let mut cfg = AutosaveStartupConfig::new();
    cfg.request_file_paths.push(dir.path().to_path_buf());
    cfg.save_file_path = Some(dir.path().to_path_buf());
    Arc::new(Mutex::new(cfg))
}

fn build(
    rt: &tokio::runtime::Runtime,
    holder: &Arc<Mutex<AutosaveStartupConfig>>,
) -> AutosaveManager {
    let builder = holder.lock().unwrap().into_builder();
    rt.block_on(builder.build())
}

/// Run `create_monitor_set("settings.req", period)` through the real
/// command handler and return the interval the set was built with.
fn monitor_interval(period: i64) -> Duration {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let holder = holder_with_req(&dir);
    let cmds = AutosaveStartupConfig::register_startup_commands(holder.clone());
    let ctx = make_ctx(&rt);

    find(&cmds, "create_monitor_set")
        .handler
        .call(
            &[
                ArgValue::String("settings.req".to_string()),
                ArgValue::Int(period),
            ],
            &ctx,
        )
        .expect("create_monitor_set must accept the period the shell parsed");

    let mgr = build(&rt, &holder);
    match &mgr.sets()[0].0.config().strategy {
        SaveStrategy::Periodic { interval } => *interval,
        other => panic!("expected a periodic set, got {other:?}"),
    }
}

/// Zero is what a bare `create_monitor_set("file.req", 0)` types, and it
/// is the value that panics the timer.
#[test]
fn a_zero_period_monitor_set_gets_a_legal_interval() {
    assert_eq!(monitor_interval(0), Duration::from_secs(1));
}

/// The other end: a negative period must not become an unsigned age.
#[test]
fn a_negative_period_monitor_set_gets_a_legal_interval() {
    assert_eq!(monitor_interval(-1), Duration::from_secs(1));
}

/// A period the operator meant is passed through untouched — the clamp
/// is a floor, not a rewrite.
#[test]
fn a_sane_period_monitor_set_is_left_alone() {
    assert_eq!(monitor_interval(30), Duration::from_secs(30));
}

/// The second loop no longer reads that field at all.
/// `create_triggered_set` takes no period in C either — the trigger is a
/// CA monitor — so the watcher interval is a constant and cannot be the
/// monitor set's number under another meaning, which is what one shared
/// `period` field made it.
#[test]
fn the_triggered_loop_takes_no_period() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let holder = holder_with_req(&dir);
    let cmds = AutosaveStartupConfig::register_startup_commands(holder.clone());
    let ctx = make_ctx(&rt);

    find(&cmds, "create_monitor_set")
        .handler
        .call(
            &[
                ArgValue::String("settings.req".to_string()),
                ArgValue::Int(30),
            ],
            &ctx,
        )
        .unwrap();
    find(&cmds, "create_triggered_set")
        .handler
        .call(
            &[
                ArgValue::String("settings.req".to_string()),
                ArgValue::String("IOC:saveTrigger".to_string()),
            ],
            &ctx,
        )
        .unwrap();

    let mgr = build(&rt, &holder);
    let sets = mgr.sets();
    assert_eq!(sets.len(), 2, "one monitor set and one triggered set");

    let periodic = match &sets[0].0.config().strategy {
        SaveStrategy::Periodic { interval } => *interval,
        other => panic!("expected the monitor set first, got {other:?}"),
    };
    let debounce = match &sets[1].0.config().strategy {
        SaveStrategy::Triggered { poll_interval, .. } => *poll_interval,
        other => panic!("expected the triggered set second, got {other:?}"),
    };
    assert_eq!(
        periodic,
        Duration::from_secs(30),
        "the monitor set keeps the period the operator typed"
    );
    assert_eq!(
        debounce,
        Duration::from_secs(1),
        "the trigger watcher's interval is its own, not the monitor period"
    );
}
