//! One save set's construction failure must not take the other sets'
//! coverage with it.
//!
//! `AutosaveBuilder::build` used to `?` out of the loop that constructs
//! the sets, so a single `.req` line naming an undefined macro discarded
//! every set already constructed and the IOC came up saving nothing at
//! all — with `fdblist` listing nothing to say which set was at fault.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_file::read_save_file;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveSetStatus, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

fn no_backup() -> BackupConfig {
    BackupConfig {
        enable_savb: false,
        num_seq_files: 0,
        seq_period: Duration::from_secs(60),
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    }
}

fn manual_set(name: &str, save_path: std::path::PathBuf, req: std::path::PathBuf) -> SaveSetConfig {
    SaveSetConfig {
        name: name.into(),
        save_path,
        strategy: SaveStrategy::Manual,
        request_file: Some(req),
        request_pvs: Vec::new(),
        backup: no_backup(),
        macros: HashMap::new(),
        search_paths: Vec::new(),
    }
}

/// The valid set is built, saves, and reports `Idle`; the set whose
/// `.req` names an undefined macro is dropped on its own and reports
/// `Error`. Both halves matter: the first is the coverage that used to
/// be lost, the second is the operator's only way to learn it was.
#[epics_macros_rs::epics_test]
async fn a_failing_save_set_does_not_take_the_others_with_it() {
    let dir = tempfile::tempdir().unwrap();

    let good_req = dir.path().join("auto_settings.req");
    epics_base_rs::runtime::fs::write(&good_req, "TEMP\n")
        .await
        .unwrap();
    // `expand` falls back to the environment before it calls a macro
    // undefined, so the key has to be one no environment defines.
    let bad_req = dir.path().join("auto_positions.req");
    epics_base_rs::runtime::fs::write(&bad_req, "$(AUTOSAVE_ISOLATION_UNDEFINED)mtr1.RBV\n")
        .await
        .unwrap();

    let good_sav = dir.path().join("auto_settings.sav");

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(25.5)))
        .await
        .unwrap();

    // Valid set first, so a build that abandons the collection on the
    // first failure discards a set it had already constructed.
    let mgr = AutosaveBuilder::new()
        .add_set(manual_set("auto_settings", good_sav.clone(), good_req))
        .add_set(manual_set(
            "auto_positions",
            dir.path().join("auto_positions.sav"),
            bad_req,
        ))
        .build()
        .await;

    assert_eq!(
        mgr.set_names(),
        vec!["auto_settings".to_string()],
        "the valid set must survive the invalid one"
    );

    let saved = mgr
        .manual_save("auto_settings", &db)
        .await
        .expect("the surviving set must still save");
    assert_eq!(saved, 1);
    let entries = read_save_file(&good_sav).await.unwrap().unwrap().entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].pv_name, "TEMP");

    // `fdblist` and `asStatus` both read `status_all`, so this is what an
    // operator sees.
    let statuses = mgr.status_all().await;
    let names: Vec<&str> = statuses.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["auto_settings", "auto_positions"]);
    assert!(
        matches!(statuses[0].1, SaveSetStatus::Idle),
        "built set: {:?}",
        statuses[0].1
    );
    match &statuses[1].1 {
        SaveSetStatus::Error(reason) => assert!(
            reason.contains("AUTOSAVE_ISOLATION_UNDEFINED"),
            "the reason must name the macro that failed: {reason}"
        ),
        other => panic!("the unbuilt set must report an error status, got {other:?}"),
    }
}

/// The set that failed has no state, so it must not be reachable through
/// the by-name lookups that expect one — `manual_save` on it is a
/// not-found, not a panic and not a silent success.
#[epics_macros_rs::epics_test]
async fn an_unbuilt_save_set_is_not_savable_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let bad_req = dir.path().join("bad.req");
    epics_base_rs::runtime::fs::write(&bad_req, "$(AUTOSAVE_ISOLATION_UNDEFINED)x\n")
        .await
        .unwrap();

    let db = Arc::new(PvDatabase::new());
    let mgr = AutosaveBuilder::new()
        .add_set(manual_set("bad", dir.path().join("bad.sav"), bad_req))
        .build()
        .await;

    assert!(mgr.set_names().is_empty());
    assert!(mgr.manual_save("bad", &db).await.is_err());
    assert!(!dir.path().join("bad.sav").exists());
}
