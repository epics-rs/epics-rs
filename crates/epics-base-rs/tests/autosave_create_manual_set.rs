//! `create_manual_set(filename, macrostring)` (save_restore.c:2311): a set
//! that saves only on request, before or after `iocInit`; an empty filename
//! creates nothing and is not an error.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::save_set::SaveStrategy;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext, CommandDef};
use epics_base_rs::server::records::ao::AoRecord;

fn find<'a>(cmds: &'a [CommandDef], name: &str) -> &'a CommandDef {
    cmds.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not registered as an iocsh command"))
}

fn holder(dir: &tempfile::TempDir) -> Arc<Mutex<AutosaveStartupConfig>> {
    std::fs::write(dir.path().join("settings.req"), "$(P)setpoint\n").unwrap();
    let mut cfg = AutosaveStartupConfig::new();
    cfg.request_file_paths.push(dir.path().to_path_buf());
    cfg.save_file_path = Some(dir.path().to_path_buf());
    Arc::new(Mutex::new(cfg))
}

/// Run one command on a plain thread, as the shell does: after `iocInit`
/// it blocks on the running manager.
fn run(cmds: Arc<Vec<CommandDef>>, db: Arc<PvDatabase>, args: Vec<ArgValue>) {
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    std::thread::spawn(move || {
        let ctx = CommandContext::new(db, bridge);
        find(&cmds, "create_manual_set")
            .handler
            .call(&args, &ctx)
            .expect("create_manual_set");
    })
    .join()
    .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_set_created_before_ioc_init_saves_only_on_request() {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    let cmds = Arc::new(AutosaveStartupConfig::register_startup_commands(
        holder.clone(),
    ));
    let db = Arc::new(PvDatabase::new());
    db.add_record("IOC:setpoint", Box::new(AoRecord::new(1.5)))
        .await
        .unwrap();

    run(cmds.clone(), db.clone(), vec![ArgValue::String("".into())]);
    assert!(holder.lock().unwrap().sets.is_empty());
    run(
        cmds,
        db.clone(),
        vec![
            ArgValue::String("settings.req".into()),
            ArgValue::String("P=IOC:".into()),
        ],
    );

    let builder = holder.lock().unwrap().into_builder();
    let mgr = builder.build().await;
    let sets = mgr.sets();
    assert_eq!(sets.len(), 1);
    let set = &sets[0].0;
    assert!(matches!(set.methods()[..], [SaveStrategy::Manual]));
    assert_eq!(set.pv_names(), vec!["IOC:setpoint"]);

    let (set, _) = mgr.find_set_by_request_file("settings.req").unwrap();
    mgr.manual_save(&set.config().name, &db).await.unwrap();
    let sav = std::fs::read_to_string(dir.path().join("settings.sav")).unwrap();
    assert!(sav.contains("IOC:setpoint"), "{sav}");
}

#[epics_macros_rs::epics_test]
async fn a_set_created_after_ioc_init_joins_the_running_manager() {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    let cmds = Arc::new(AutosaveStartupConfig::register_startup_commands(
        holder.clone(),
    ));
    let db = Arc::new(PvDatabase::new());
    let builder = holder.lock().unwrap().into_builder();
    let mgr = Arc::new(builder.build().await);
    holder.lock().unwrap().manager = Some(mgr.clone());

    run(
        cmds,
        db,
        vec![
            ArgValue::String("settings.req".into()),
            ArgValue::String("P=IOC:".into()),
        ],
    );
    assert!(holder.lock().unwrap().sets.is_empty());
    let (set, _) = mgr.find_set_by_request_file("settings.req").unwrap();
    assert!(matches!(set.methods()[..], [SaveStrategy::Manual]));
}
