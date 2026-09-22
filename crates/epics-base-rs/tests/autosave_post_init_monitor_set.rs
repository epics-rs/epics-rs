// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both tests hand-build their own tokio runtime and passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_post_init_monitor_set` (2/2).
//! `create_monitor_set` after `iocInit` creates a set that saves.
//!
//! C's `create_data_set` (`save_restore.c`) appends to the live set list
//! whenever it is called, so an `st.cmd` that creates its sets after
//! `iocInit` gets an IOC that autosaves. The port used to consume the
//! startup config once, at `iocInit`, and a later `create_monitor_set`
//! appended to a list nothing read again: the command printed its banner
//! and no `.sav` was ever written.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext, CommandDef};
use epics_base_rs::server::records::ao::AoRecord;

fn find<'a>(cmds: &'a [CommandDef], name: &str) -> &'a CommandDef {
    cmds.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not registered as an iocsh command"))
}

#[test]
fn create_monitor_set_after_ioc_init_starts_a_saving_set() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.req"), "IOC:setpoint\n").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut cfg = AutosaveStartupConfig::new();
    cfg.request_file_paths.push(dir.path().to_path_buf());
    cfg.save_file_path = Some(dir.path().to_path_buf());
    let holder = Arc::new(Mutex::new(cfg));
    let cmds = AutosaveStartupConfig::register_startup_commands(holder.clone());

    // `iocInit`: the manager is built from a config with no set in it and
    // started by the server, then handed back to the config.
    let db = Arc::new(PvDatabase::new());
    let (bridge, _autosave_task) = rt.block_on(async {
        db.add_record("IOC:setpoint", Box::new(AoRecord::new(1.5)))
            .await
            .unwrap();
        let builder = holder.lock().unwrap().into_builder();
        let mgr = Arc::new(builder.build().await);
        assert!(mgr.set_names().is_empty());
        let reactor = epics_base_rs::runtime::task::Reactor::current().unwrap();
        let task = mgr.clone().start(&reactor, db.clone());
        holder.lock().unwrap().manager = Some(mgr);
        (
            epics_base_rs::runtime::task::BlockingBridge::capture(),
            task,
        )
    });
    let ctx = CommandContext::new(db, bridge);

    find(&cmds, "create_monitor_set")
        .handler
        .call(
            &[
                ArgValue::String("settings.req".to_string()),
                ArgValue::Int(1),
            ],
            &ctx,
        )
        .expect("create_monitor_set after iocInit is accepted");

    let mgr = holder.lock().unwrap().manager.clone().unwrap();
    assert_eq!(mgr.set_names(), vec!["settings.req".to_string()]);
    assert!(
        holder.lock().unwrap().monitor_sets.is_empty(),
        "a set added to the running manager is not queued for a build that already happened"
    );

    // The set's task ticks on its own; C's `create_data_set` starts the
    // save task the same way.
    let sav = dir.path().join("settings.sav");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sav.exists() {
        assert!(Instant::now() < deadline, "no settings.sav after 10 s");
        std::thread::sleep(Duration::from_millis(100));
    }
    let text = std::fs::read_to_string(&sav).unwrap();
    assert!(text.contains("IOC:setpoint"), "{text}");
    mgr.shutdown();
}

#[test]
fn create_monitor_set_after_ioc_init_reports_an_unloadable_request_file() {
    let dir = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut cfg = AutosaveStartupConfig::new();
    cfg.request_file_paths.push(dir.path().to_path_buf());
    let holder = Arc::new(Mutex::new(cfg));
    let cmds = AutosaveStartupConfig::register_startup_commands(holder.clone());
    let db = Arc::new(PvDatabase::new());
    let bridge = rt.block_on(async {
        let builder = holder.lock().unwrap().into_builder();
        let mgr = Arc::new(builder.build().await);
        holder.lock().unwrap().manager = Some(mgr);
        epics_base_rs::runtime::task::BlockingBridge::capture()
    });
    let ctx = CommandContext::new(db, bridge);

    let err = match find(&cmds, "create_monitor_set").handler.call(
        &[
            ArgValue::String("missing.req".to_string()),
            ArgValue::Int(1),
        ],
        &ctx,
    ) {
        Err(e) => e,
        Ok(_) => panic!("a request file that cannot be found is the caller's error"),
    };
    assert!(err.contains("missing.req"), "{err}");
    let mgr = holder.lock().unwrap().manager.clone().unwrap();
    assert!(mgr.set_names().is_empty());
    assert_eq!(
        rt.block_on(mgr.status_all()).len(),
        1,
        "listed by fdblist as an error"
    );
}
