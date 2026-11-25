//! C `create_data_set` holds the method that makes a list to
//! `MAX(period, MIN_PERIOD)` (save_restore.c:2422, 2433), but stores the
//! period of a method added to an existing list as given (:2384-2389).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::manager::AutosaveManager;
use epics_base_rs::server::autosave::save_set::SaveStrategy;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext, CommandDef};

fn holder(dir: &tempfile::TempDir) -> Arc<Mutex<AutosaveStartupConfig>> {
    std::fs::write(dir.path().join("settings.req"), "IOC:setpoint\n").unwrap();
    let mut cfg = AutosaveStartupConfig::new();
    cfg.request_file_paths.push(dir.path().to_path_buf());
    cfg.save_file_path = Some(dir.path().to_path_buf());
    Arc::new(Mutex::new(cfg))
}

/// Run one command on a plain thread, as the shell does.
fn run(holder: &Arc<Mutex<AutosaveStartupConfig>>, cmd: &str, period: i64) {
    let cmds: Vec<CommandDef> = AutosaveStartupConfig::register_startup_commands(holder.clone());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let cmd = cmd.to_string();
    std::thread::spawn(move || {
        let ctx = CommandContext::new(Arc::new(PvDatabase::new()), bridge);
        let def = cmds.iter().find(|c| c.name == cmd).expect("registered");
        let args = [
            ArgValue::String("settings.req".into()),
            ArgValue::Int(period),
        ];
        def.handler.call(&args, &ctx).expect("accepted");
    })
    .join()
    .unwrap();
}

fn periods(mgr: &AutosaveManager) -> (Duration, Duration) {
    let methods = mgr.sets()[0].0.methods();
    let periodic = methods.iter().find_map(|m| match m {
        SaveStrategy::Periodic { interval } => Some(*interval),
        _ => None,
    });
    let monitor = methods.iter().find_map(|m| match m {
        SaveStrategy::OnChange { min_interval } => Some(*min_interval),
        _ => None,
    });
    (periodic.unwrap(), monitor.unwrap())
}

const S: fn(u64) -> Duration = Duration::from_secs;

#[epics_macros_rs::epics_test]
async fn a_method_added_before_ioc_init_keeps_its_period() {
    for (first, added, want) in [
        ("create_periodic_set", 2, (S(4), S(2))),
        ("create_monitor_set", 0, (S(0), S(4))),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let holder = holder(&dir);
        let other = if first == "create_periodic_set" {
            "create_monitor_set"
        } else {
            "create_periodic_set"
        };
        run(&holder, first, 1);
        run(&holder, other, added);
        let mgr = holder.lock().unwrap().manager.clone().unwrap();
        assert_eq!(periods(&mgr), want, "{first}");
    }
}

#[epics_macros_rs::epics_test]
async fn a_method_added_after_ioc_init_keeps_its_period() {
    for (first, added, want) in [
        ("create_periodic_set", 2, (S(4), S(2))),
        ("create_monitor_set", -5, (S(0), S(4))),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let holder = holder(&dir);
        let other = if first == "create_periodic_set" {
            "create_monitor_set"
        } else {
            "create_periodic_set"
        };
        let builder = holder.lock().unwrap().into_builder();
        let mgr = Arc::new(builder.build().await);
        holder.lock().unwrap().manager = Some(mgr.clone());
        run(&holder, first, 1);
        run(&holder, other, added);
        assert_eq!(periods(&mgr), want, "{first}");
    }
}
