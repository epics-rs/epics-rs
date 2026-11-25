//! C `create_data_set` (save_restore.c:2367-2392): a request file is one
//! list. Each `create_*_set` naming it ORs its method into the list's
//! `save_method`, and a method the list already has is refused with
//!
//! ```text
//! ../save_restore.c:create_data_set 'settings.req' already in c mode
//! ```
//!
//! on the error log, the command itself returning normally.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::manager::AutosaveManager;
use epics_base_rs::server::autosave::save_set::SaveStrategy;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext, CommandDef};
use epics_base_rs::server::records::ao::AoRecord;

const ALL: u32 = SaveStrategy::PERIODIC
    | SaveStrategy::MONITORED
    | SaveStrategy::TRIGGERED
    | SaveStrategy::MANUAL;

fn holder(dir: &tempfile::TempDir) -> Arc<Mutex<AutosaveStartupConfig>> {
    std::fs::write(dir.path().join("settings.req"), "$(P)setpoint\n").unwrap();
    std::fs::write(dir.path().join("other.req"), "IOC:trig\n").unwrap();
    let mut cfg = AutosaveStartupConfig::new();
    cfg.request_file_paths.push(dir.path().to_path_buf());
    cfg.save_file_path = Some(dir.path().to_path_buf());
    Arc::new(Mutex::new(cfg))
}

async fn db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    for name in ["IOC:setpoint", "X:setpoint", "IOC:trig"] {
        db.add_record(name, Box::new(AoRecord::new(1.5)))
            .await
            .unwrap();
    }
    db
}

/// Run one command on a plain thread, as the shell does; it must not fail.
fn run(holder: &Arc<Mutex<AutosaveStartupConfig>>, db: &Arc<PvDatabase>, cmd: &str, args: &[&str]) {
    let cmds: Vec<CommandDef> = AutosaveStartupConfig::register_startup_commands(holder.clone());
    let args: Vec<ArgValue> = args
        .iter()
        .map(|a| match a.parse::<i64>() {
            Ok(n) => ArgValue::Int(n),
            Err(_) => ArgValue::String((*a).into()),
        })
        .collect();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let db = db.clone();
    let cmd = cmd.to_string();
    std::thread::spawn(move || {
        let ctx = CommandContext::new(db, bridge);
        let def = cmds.iter().find(|c| c.name == cmd).expect("registered");
        def.handler
            .call(&args, &ctx)
            .expect("C's command never fails");
    })
    .join()
    .unwrap();
}

fn create_all(holder: &Arc<Mutex<AutosaveStartupConfig>>, db: &Arc<PvDatabase>) {
    run(
        holder,
        db,
        "create_periodic_set",
        &["settings.req", "30", "P=IOC:"],
    );
    run(
        holder,
        db,
        "create_monitor_set",
        &["settings.req", "5", "P=X:"],
    );
    run(
        holder,
        db,
        "create_triggered_set",
        &["settings.req", "IOC:trig", "P=X:"],
    );
    run(holder, db, "create_manual_set", &["settings.req", "P=X:"]);
}

fn heard() -> Arc<Mutex<Vec<String>>> {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = heard.clone();
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.trim_end().to_string()));
    heard
}

fn refusals(heard: &Mutex<Vec<String>>) -> Vec<String> {
    errlog_flush();
    heard
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("already in"))
        .cloned()
        .collect()
}

fn expected_refusals() -> Vec<String> {
    ["1", "c", "2", "10"]
        .iter()
        .map(|bit| {
            format!("../save_restore.c:create_data_set 'settings.req' already in {bit} mode")
        })
        .collect()
}

/// The manager the first `create_*_set` started.
fn manager(holder: &Mutex<AutosaveStartupConfig>) -> Arc<AutosaveManager> {
    holder.lock().unwrap().manager.clone().expect("started")
}

fn assert_one_set_with_every_method(mgr: &AutosaveManager) {
    let sets = mgr.sets();
    assert_eq!(sets.len(), 1, "one request file, one list");
    let set = &sets[0].0;
    assert_eq!(set.save_method(), ALL);
    assert_eq!(set.save_file(), "settings.sav");
    assert_eq!(
        set.pv_names(),
        vec!["IOC:setpoint"],
        "the first call's macros are the list's"
    );
}

#[epics_macros_rs::epics_test]
async fn methods_named_before_ioc_init_join_one_set() {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    let db = db().await;
    create_all(&holder, &db);
    assert_one_set_with_every_method(&manager(&holder));
}

#[epics_macros_rs::epics_test]
async fn methods_named_after_ioc_init_join_the_running_set() {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    let db = db().await;
    let builder = holder.lock().unwrap().into_builder();
    let mgr = Arc::new(builder.build().await);
    holder.lock().unwrap().manager = Some(mgr.clone());
    create_all(&holder, &db);
    assert_one_set_with_every_method(&mgr);
}

#[epics_macros_rs::epics_test]
async fn a_method_twice_is_refused_with_cs_line() {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    let db = db().await;
    let heard = heard();
    create_all(&holder, &db);
    assert!(refusals(&heard).is_empty());
    create_all(&holder, &db);
    assert_eq!(refusals(&heard), expected_refusals());
    assert_one_set_with_every_method(&manager(&holder));
}

#[epics_macros_rs::epics_test]
async fn different_request_files_are_different_sets() {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    let db = db().await;
    run(
        &holder,
        &db,
        "create_manual_set",
        &["settings.req", "P=IOC:"],
    );
    run(&holder, &db, "create_manual_set", &["other.req", ""]);
    let mgr = manager(&holder);
    assert_eq!(mgr.set_names(), vec!["settings.req", "other.req"]);
}

/// C frees the list whose request file did not open (save_restore.c:
/// 2460-2462), so nothing collects methods: a repeat of the same method
/// is a fresh attempt, not a refusal, and once the file is there a call
/// builds the list.
#[epics_macros_rs::epics_test]
async fn a_set_that_did_not_build_is_not_kept() {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    let db = db().await;
    let heard = heard();
    let builder = holder.lock().unwrap().into_builder();
    let mgr = Arc::new(builder.build().await);
    holder.lock().unwrap().manager = Some(mgr.clone());
    run(&holder, &db, "create_manual_set", &["missing.req"]);
    run(
        &holder,
        &db,
        "create_triggered_set",
        &["missing.req", "IOC:trig"],
    );
    run(&holder, &db, "create_manual_set", &["missing.req"]);
    assert!(refusals(&heard).is_empty());
    assert!(mgr.status_all().await.is_empty());

    std::fs::write(dir.path().join("missing.req"), "IOC:trig\n").unwrap();
    run(&holder, &db, "create_manual_set", &["missing.req"]);
    let sets = mgr.sets();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].0.save_method(), SaveStrategy::MANUAL);
}

/// A method added to a running set is enabled by the next pass: the
/// trigger saves the manual set's one `.sav`.
#[epics_macros_rs::epics_test]
async fn a_method_added_to_a_running_set_starts_its_task() {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    let db = db().await;
    run(
        &holder,
        &db,
        "create_manual_set",
        &["settings.req", "P=IOC:"],
    );
    let mgr = manager(&holder);
    run(
        &holder,
        &db,
        "create_triggered_set",
        &["settings.req", "IOC:trig"],
    );

    let sav = dir.path().join("settings.sav");
    // The trigger's first monitor event, its value at enable, saves.
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if sav.exists() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    }
    std::fs::remove_file(&sav).expect("the first event saved the set");
    // A CA put: `dbPutField` processes the record, which posts.
    db.put_record_field_from_ca_no_notify(
        "IOC:trig",
        "VAL",
        epics_base_rs::types::EpicsValue::Double(2.0),
    )
    .await
    .unwrap();
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if sav.exists() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    }
    let text = std::fs::read_to_string(&sav).expect("the trigger saved the set");
    assert!(text.contains("IOC:setpoint"), "{text}");
    mgr.shutdown();
}
