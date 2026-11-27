//! A save set that could not load its member list must not be scheduled to
//! write over the file holding the previous run's values.
//!
//! `SaveSetConfig::request_file` is `Option<PathBuf>`, and the `None` had
//! two meanings: "this set has no request file, its members are
//! `request_pvs`" and "this set named one, but it could not be found".
//! The startup config's set builder produced the second by
//! pre-resolving the name and dropping the failure, so a typo'd
//! `create_monitor_set("auto_setting.req", 5)` built a set with zero
//! entries. `SaveSet::save_once` then rotated the good `.sav` into `.savB`,
//! wrote a header-plus-`<END>` file over it, and on the next cycle rotated
//! THAT over `.savB` too — both generations of a real save set gone, with
//! nothing on stderr, because `write_save_file` appends `<END>`
//! unconditionally and `validate_save_file` accepts the result.
//!
//! The name now travels to `load_request_file_with_search_paths`, which
//! owns the search and reports `AutosaveError::RequestFile`, so the `None`
//! has one meaning again and the bad set cannot be built at all.
//!
//! No C reference: synApps `save_restore.c` is not present on this
//! machine, so these pin the port's own stated invariant.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::manager::AutosaveManager;
use epics_base_rs::server::autosave::save_set::SaveSetStatus;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext};

/// A `.sav` from a previous run: real values, and a valid `<END>`.
const GOOD_SAV: &str = "# autosave-rs V1.0\nIOC:setpoint 42.5\nIOC:enable 1\n<END>\n";

/// Run `cmd(args)` on `cfg` on a plain thread, as the shell does, and
/// return the manager the first `create_*_set` started.
fn run(cfg: AutosaveStartupConfig, lines: &[(&str, &str, ArgValue)]) -> Arc<AutosaveManager> {
    let holder = Arc::new(Mutex::new(cfg));
    let cmds = AutosaveStartupConfig::register_startup_commands(holder.clone());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let lines: Vec<(String, Vec<ArgValue>)> = lines
        .iter()
        .map(|(cmd, file, arg)| {
            let args = vec![ArgValue::String((*file).into()), arg.clone()];
            (cmd.to_string(), args)
        })
        .collect();
    std::thread::spawn(move || {
        let ctx = CommandContext::new(Arc::new(PvDatabase::new()), bridge);
        for (cmd, args) in lines {
            let def = cmds.iter().find(|c| c.name == cmd).expect("registered");
            def.handler
                .call(&args, &ctx)
                .expect("C's command never fails");
        }
    })
    .join()
    .unwrap();
    let mgr = holder.lock().unwrap().manager.clone();
    mgr.expect("started")
}

fn monitor_set(filename: &str) -> (&str, &str, ArgValue) {
    ("create_monitor_set", filename, ArgValue::Int(5))
}

/// A startup config whose request-file search path and save-file directory
/// are `dir`, holding a good `.sav` for `stem` and no request file at all.
fn config_with_good_sav(dir: &std::path::Path, stem: &str) -> (AutosaveStartupConfig, PathBuf) {
    let sav = dir.join(format!("{stem}.sav"));
    std::fs::write(&sav, GOOD_SAV).unwrap();
    let mut cfg = AutosaveStartupConfig::new();
    cfg.request_file_paths.push(dir.to_path_buf());
    cfg.save_file_path = Some(dir.to_path_buf());
    (cfg, sav)
}

/// C frees a list whose request file did not open (save_restore.c:
/// 2460-2462): nothing of it is left for `save_restoreShow` to list.
async fn assert_not_kept(mgr: &AutosaveManager) {
    assert!(mgr.set_names().is_empty());
    assert!(mgr.status_all().await.is_empty());
}

#[epics_macros_rs::epics_test]
async fn a_monitor_set_naming_a_missing_request_file_does_not_build() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, sav) = config_with_good_sav(dir.path(), "auto_settings");
    let mgr = run(cfg, &[monitor_set("auto_settings.req")]);
    assert_not_kept(&mgr).await;
    assert_eq!(
        std::fs::read_to_string(&sav).unwrap(),
        GOOD_SAV,
        "the previous run's values must still be on disk"
    );
}

/// The sibling arm. `create_triggered_set` builds its config in a second
/// loop, and the pre-resolution was copied into both.
#[epics_macros_rs::epics_test]
async fn a_triggered_set_naming_a_missing_request_file_does_not_build() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, sav) = config_with_good_sav(dir.path(), "trig");
    let trigger = ArgValue::String("IOC:saveTrigger".into());
    let mgr = run(cfg, &[("create_triggered_set", "trig.req", trigger)]);
    assert_not_kept(&mgr).await;
    assert_eq!(std::fs::read_to_string(&sav).unwrap(), GOOD_SAV);
}

/// The refusal is per set. The set that would have destroyed its `.sav`
/// is the only one dropped; the set that loaded is scheduled as usual.
#[epics_macros_rs::epics_test]
async fn a_missing_request_file_costs_only_its_own_set() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, sav) = config_with_good_sav(dir.path(), "present");
    std::fs::write(dir.path().join("present.req"), "IOC:setpoint\n").unwrap();
    let mgr = run(
        cfg,
        &[monitor_set("present.req"), monitor_set("absent.req")],
    );
    assert_eq!(mgr.set_names(), vec!["present.req".to_string()]);
    let statuses = mgr.status_all().await;
    assert_eq!(statuses.len(), 1, "C keeps no list for absent.req");
    assert!(matches!(statuses[0].1, SaveSetStatus::Idle));
    assert_eq!(std::fs::read_to_string(&sav).unwrap(), GOOD_SAV);
}

/// The negative control: the `None` still has its one legitimate meaning,
/// and a request file that DOES resolve still builds and carries its PVs.
#[epics_macros_rs::epics_test]
async fn a_resolvable_request_file_still_builds_with_its_members() {
    let dir = tempfile::tempdir().unwrap();
    let (cfg, _sav) = config_with_good_sav(dir.path(), "present");
    std::fs::write(dir.path().join("present.req"), "IOC:setpoint\nIOC:enable\n").unwrap();
    let mgr = run(cfg, &[monitor_set("present.req")]);
    let sets = mgr.sets();
    assert_eq!(sets.len(), 1);
    assert_eq!(
        sets[0].0.pv_names(),
        vec!["IOC:setpoint".to_string(), "IOC:enable".to_string()],
        "the set must carry the members the request file declared"
    );
}
