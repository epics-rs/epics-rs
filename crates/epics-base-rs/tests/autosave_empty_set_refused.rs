//! A save set with no members must not exist.
//!
//! `save_once` does not consult the member list before it starts: it
//! rotates the previous `.sav` into `.savB` and the sequence slots
//! (`validate_save_file` accepts them - it only looks for `<END>`), then
//! renames a header-plus-`<END>` file over the `.sav` itself, because
//! `write_save_file_with_mode` has no empty-entry case. With
//! `BackupConfig::default()` every generation is overwritten within
//! `num_seq_files * seq_period`, and the next boot restores zero PVs,
//! reports success, and brings the IOC up on its `.db` defaults.
//!
//! No strategy guarded against this, so the fix is not a guard: an empty
//! member list is refused where the list is built, which keeps the set
//! out of the manager entirely and out of every strategy's reach.
//!
//! No C reference: synApps `save_restore.c` is not present on this
//! machine, so these pin the port's own stated invariant.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::error::AutosaveError;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{
    SaveSet, SaveSetConfig, SaveSetStatus, SaveStrategy,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

/// A previous run's file: real values, valid `<END>`.
const GOOD_SAV: &str = "# autosave-rs V1.0\nIOC:setpoint 42.5\nIOC:enable 1\n<END>\n";

/// The stock backup policy - the one that turns a single empty write
/// into the loss of every generation.
fn default_backup() -> BackupConfig {
    BackupConfig::default()
}

async fn db_with_setpoint() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("IOC:setpoint", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db
}

/// The operator-visible shape: a `.req` that resolves but declares no
/// PVs (all comments). The set must not build, and the file holding the
/// previous run's values must still be byte-identical afterwards.
#[epics_macros_rs::epics_test]
async fn a_request_file_declaring_no_pvs_never_reaches_a_save_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let req = dir.path().join("auto_settings.req");
    std::fs::write(&req, "# every line here is a comment\n#IOC:setpoint\n").unwrap();
    let sav = dir.path().join("auto_settings.sav");
    std::fs::write(&sav, GOOD_SAV).unwrap();

    let db = db_with_setpoint().await;
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .add_set(SaveSetConfig {
                name: "auto_settings".into(),
                save_path: sav.clone(),
                strategy: SaveStrategy::Periodic {
                    interval: Duration::from_millis(20),
                },
                request_file: Some(req.clone()),
                request_pvs: Vec::new(),
                backup: default_backup(),
                macros: HashMap::new(),
                search_paths: vec![dir.path().to_path_buf()],
            })
            .build()
            .await,
    );

    // Run the periodic task for several intervals first: the data loss
    // is what this pins, so it is what has to be asserted before the
    // reason it cannot happen.
    let handle = mgr.clone().start(db.clone());
    epics_base_rs::runtime::task::sleep(Duration::from_millis(120)).await;
    mgr.shutdown();
    let _ = handle.await;

    assert_eq!(
        std::fs::read_to_string(&sav).unwrap(),
        GOOD_SAV,
        "the previous run's values must survive"
    );
    for ext in ["savB", "sav0", "sav1", "sav2"] {
        assert!(
            !sav.with_extension(ext).exists(),
            "no backup generation may be written for a set that never saved: .{ext}"
        );
    }

    assert!(
        mgr.set_names().is_empty(),
        "a set that would save nothing must not build"
    );
    match &mgr.status_all().await[0].1 {
        SaveSetStatus::Error(text) => assert!(
            text.contains("auto_settings.req") && text.contains("no PVs"),
            "the refusal must name the file and the reason, got: {text}"
        ),
        other => panic!("the refused set must carry an error status, got {other:?}"),
    }
}

/// The programmatic shape: no request file and no inline PVs. The
/// reason has to say which of the two sources was missing, because the
/// operator's next move differs.
#[epics_macros_rs::epics_test]
async fn a_set_with_no_members_at_all_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = SaveSetConfig {
        name: "nothing".into(),
        save_path: dir.path().join("nothing.sav"),
        strategy: SaveStrategy::Manual,
        request_file: None,
        request_pvs: Vec::new(),
        backup: default_backup(),
        macros: HashMap::new(),
        search_paths: Vec::new(),
    };

    match SaveSet::new(cfg).await {
        Err(AutosaveError::EmptySaveSet { name, reason }) => {
            assert_eq!(name, "nothing");
            assert!(reason.contains("no request file"), "got: {reason}");
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("a set with no members must not be constructible"),
    }
}

/// The set is built with members, then the `.req` is emptied under it.
/// `reload_request` must refuse to install the empty list rather than
/// turn a running set into one that erases its own file.
#[epics_macros_rs::epics_test]
async fn reload_request_cannot_empty_a_live_set() {
    let dir = tempfile::tempdir().unwrap();
    let req = dir.path().join("live.req");
    std::fs::write(&req, "IOC:setpoint\n").unwrap();

    let mut set = SaveSet::new(SaveSetConfig {
        name: "live".into(),
        save_path: dir.path().join("live.sav"),
        strategy: SaveStrategy::Manual,
        request_file: Some(req.clone()),
        request_pvs: Vec::new(),
        backup: default_backup(),
        macros: HashMap::new(),
        search_paths: vec![dir.path().to_path_buf()],
    })
    .await
    .expect("a set with one member builds");
    assert_eq!(set.pv_names(), vec!["IOC:setpoint".to_string()]);

    std::fs::write(&req, "# emptied\n").unwrap();
    assert!(
        set.reload_request().await.is_err(),
        "an emptied request file must not become an empty live set"
    );
    assert_eq!(
        set.pv_names(),
        vec!["IOC:setpoint".to_string()],
        "the members the set was built with must survive a refused reload"
    );
}
