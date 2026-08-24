//! A save set that could not load its member list must not be scheduled to
//! write over the file holding the previous run's values.
//!
//! `SaveSetConfig::request_file` is `Option<PathBuf>`, and the `None` had
//! two meanings: "this set has no request file, its members are
//! `request_pvs`" and "this set named one, but it could not be found".
//! `AutosaveStartupConfig::into_builder` produced the second by
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

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::manager::AutosaveManager;
use epics_base_rs::server::autosave::save_set::SaveSetStatus;
use epics_base_rs::server::autosave::startup::MonitorSetDef;

/// A `.sav` from a previous run: real values, and a valid `<END>`.
const GOOD_SAV: &str = "# autosave-rs V1.0\nIOC:setpoint 42.5\nIOC:enable 1\n<END>\n";

fn monitor_set(filename: &str) -> MonitorSetDef {
    MonitorSetDef {
        filename: filename.to_string(),
        period: MonitorSetDef::poll_period(5),
        macros: String::new(),
        trigger_pv: None,
    }
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

/// The reason recorded for the one set `mgr` refused to build.
async fn sole_refusal(mgr: &AutosaveManager) -> String {
    let statuses = mgr.status_all().await;
    assert_eq!(statuses.len(), 1, "one configured set, so one status");
    match &statuses[0].1 {
        SaveSetStatus::Error(text) => text.clone(),
        other => panic!("a set that did not build must report an error, got {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn a_monitor_set_naming_a_missing_request_file_does_not_build() {
    let dir = tempfile::tempdir().unwrap();
    let (mut cfg, sav) = config_with_good_sav(dir.path(), "auto_settings");
    cfg.monitor_sets.push(monitor_set("auto_settings.req"));

    let mgr = cfg.into_builder().build().await;
    assert!(
        mgr.set_names().is_empty(),
        "a set whose member list will not load must not build"
    );
    let text = sole_refusal(&mgr).await;
    assert!(
        text.contains("auto_settings.req"),
        "the error must name the file the operator typed, got: {text}"
    );
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
    let (mut cfg, sav) = config_with_good_sav(dir.path(), "trig");
    let mut def = monitor_set("trig.req");
    def.trigger_pv = Some("IOC:saveTrigger".to_string());
    cfg.triggered_sets.push(def);

    let mgr = cfg.into_builder().build().await;
    assert!(
        mgr.set_names().is_empty(),
        "the triggered loop must refuse the same set the monitor loop does"
    );
    assert!(sole_refusal(&mgr).await.contains("trig.req"));
    assert_eq!(std::fs::read_to_string(&sav).unwrap(), GOOD_SAV);
}

/// The refusal is per set, and it is visible. The set that would have
/// destroyed its `.sav` is the only one dropped; the set that loaded is
/// scheduled as usual, and the drop is on the status list rather than
/// inferred from a manager that never appeared.
#[epics_macros_rs::epics_test]
async fn a_missing_request_file_costs_only_its_own_set() {
    let dir = tempfile::tempdir().unwrap();
    let (mut cfg, sav) = config_with_good_sav(dir.path(), "present");
    std::fs::write(dir.path().join("present.req"), "IOC:setpoint\n").unwrap();
    cfg.monitor_sets.push(monitor_set("present.req"));
    cfg.monitor_sets.push(monitor_set("absent.req"));

    let mgr = cfg.into_builder().build().await;
    assert_eq!(mgr.set_names(), vec!["present.req".to_string()]);
    let statuses = mgr.status_all().await;
    assert_eq!(
        statuses.len(),
        2,
        "both configured sets must be accounted for"
    );
    assert!(matches!(statuses[0].1, SaveSetStatus::Idle));
    match &statuses[1].1 {
        SaveSetStatus::Error(text) => assert!(text.contains("absent.req"), "got: {text}"),
        other => panic!("the refused set must report an error, got {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&sav).unwrap(), GOOD_SAV);
}

/// The negative control: the `None` still has its one legitimate meaning,
/// and a request file that DOES resolve still builds and carries its PVs.
#[epics_macros_rs::epics_test]
async fn a_resolvable_request_file_still_builds_with_its_members() {
    let dir = tempfile::tempdir().unwrap();
    let (mut cfg, _sav) = config_with_good_sav(dir.path(), "present");
    std::fs::write(dir.path().join("present.req"), "IOC:setpoint\nIOC:enable\n").unwrap();
    cfg.monitor_sets.push(monitor_set("present.req"));

    let mgr = cfg.into_builder().build().await;
    let sets = mgr.sets();
    assert_eq!(sets.len(), 1);
    assert_eq!(
        sets[0].0.pv_names(),
        vec!["IOC:setpoint".to_string(), "IOC:enable".to_string()],
        "the set must carry the members the request file declared"
    );
}
