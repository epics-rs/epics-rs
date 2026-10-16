//! SR_status and SR_0_Status carry C's codes (save_restore.h:38-42), the worst list status
//! of each 1 s reporting pass. The expected posts are softIoc R7.0.10 on
//! autosave's save_restoreStatus.db (P=T:), `caput` standing in for the
//! reporter's `ca_put`:
//!
//! ```text
//! $ camonitor -t n T:SR_status
//! $ for v in 4 2 1 3 0; do caput -t T:SR_status $v; done
//! T:SR_status                     No Status UDF INVALID
//! T:SR_status                     Ok
//! T:SR_status                     Warning STATE MINOR
//! T:SR_status                     Failure STATE MAJOR
//! T:SR_status                     Seq Fail STATE MAJOR
//! T:SR_status                     No Status
//! ```
//!
//! SR_0_Status, the same mbbo, posts the same five lines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::{Reactor, TaskHandle, sleep};
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::{AutosaveBuilder, AutosaveManager};
use epics_base_rs::server::autosave::save_file::{SaveEntry, write_save_file};
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::DbFieldType;

/// save_restoreStatus.db's SR_status and SR_0_Status, verbatim, and one PV
/// to save.
const DB: &str = r#"
record(mbbo, "T:SR_status") {
  field(DTYP, "Soft Channel")
  field(NOBT, "3")
  field(ONVL, "1")
  field(TWVL, "2")
  field(THVL, "3")
  field(FRVL, "4")
  field(ZRST, "No Status")
  field(ONST, "Failure")
  field(TWST, "Warning")
  field(THST, "Seq Fail")
  field(FRST, "Ok")
  field(ONSV, "MAJOR")
  field(TWSV, "MINOR")
  field(THSV, "MAJOR")
}
record(mbbo, "T:SR_0_Status") {
  field(DTYP, "Soft Channel")
  field(NOBT, "3")
  field(ONVL, "1")
  field(TWVL, "2")
  field(THVL, "3")
  field(FRVL, "4")
  field(ZRST, "No Status")
  field(ONST, "Failure")
  field(TWST, "Warning")
  field(THST, "Seq Fail")
  field(FRST, "Ok")
  field(ONSV, "MAJOR")
  field(TWSV, "MINOR")
  field(THSV, "MAJOR")
}
record(ao, "T:SAVED") { }
"#;

/// The softIoc lines above as (value, STAT, SEVR).
const NO_STATUS: (f64, u16, u16) = (0.0, 0, 0);
const OK: (f64, u16, u16) = (4.0, 0, 0);
const WARNING: (f64, u16, u16) = (2.0, 7, 1);
const FAILURE: (f64, u16, u16) = (1.0, 7, 2);
const SEQ_FAIL: (f64, u16, u16) = (3.0, 7, 2);

struct Rig {
    db: Arc<PvDatabase>,
    mgr: Arc<AutosaveManager>,
    handle: Option<TaskHandle<()>>,
    status: EventReader,
    list: EventReader,
}

impl Rig {
    async fn new(save_path: PathBuf, backup: BackupConfig) -> Self {
        Self::with_sets(vec![("s", save_path, backup)]).await
    }

    async fn with_sets(sets: Vec<(&str, PathBuf, BackupConfig)>) -> Self {
        let (db, _) = IocBuilder::new()
            .db_string(DB, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        let sub = |name: &str| {
            db.get_record(name)
                .unwrap()
                .write()
                .add_subscriber(
                    "VAL",
                    1,
                    DbFieldType::Enum,
                    (EventMask::VALUE | EventMask::ALARM).bits(),
                )
                .unwrap()
        };
        let status = sub("T:SR_status");
        let list = sub("T:SR_0_Status");
        let mut builder = AutosaveBuilder::new().status_prefix("T:");
        for (name, save_path, backup) in sets {
            builder = builder.add_set(
                SaveSetConfig {
                    name: name.into(),
                    save_path,
                    request_file: None,
                    request_pvs: vec!["T:SAVED".into()],
                    backup,
                    macros: HashMap::new(),
                    search_paths: Vec::new(),
                },
                SaveStrategy::Manual,
            );
        }
        let mgr = Arc::new(builder.build().await);
        let handle = Some(mgr.clone().start(
            &Reactor::current().expect("the test driver enters an executor"),
            db.clone(),
        ));
        Self {
            db,
            mgr,
            handle,
            status,
            list,
        }
    }

    /// The next SR_status post as (value, STAT, SEVR).
    async fn next_status(&mut self) -> (f64, u16, u16) {
        next_post(&self.db, "T:SR_status", &mut self.status).await
    }

    /// The next SR_0_Status post.
    async fn next_list(&mut self) -> (f64, u16, u16) {
        next_post(&self.db, "T:SR_0_Status", &mut self.list).await
    }

    /// The pass before any save reports the set's INIT, then one save; both
    /// records post the same line each time.
    async fn save(&mut self) -> (f64, u16, u16) {
        assert_eq!(self.next_status().await, NO_STATUS);
        assert_eq!(self.next_list().await, NO_STATUS);
        let _ = self.mgr.manual_save("s", &self.db).await;
        let status = self.next_status().await;
        assert_eq!(self.next_list().await, status);
        status
    }

    async fn stop(mut self) {
        self.mgr.shutdown();
        let _ = self.handle.take().unwrap().await;
    }
}

async fn next_post(db: &PvDatabase, name: &str, rx: &mut EventReader) -> (f64, u16, u16) {
    for _ in 0..400 {
        if let Ok(e) = rx.try_recv() {
            let v = e
                .delivered_value(&db.get_record(name).unwrap().read(), "VAL")
                .expect("VAL reads");
            return (v.to_f64().unwrap(), e.stat(), e.sevr());
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("no post within 4 s");
}

fn backup(enable_savb: bool, num_seq_files: usize) -> BackupConfig {
    BackupConfig {
        enable_savb,
        num_seq_files,
        seq_period: Duration::from_secs(60),
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    }
}

async fn valid_sav(path: &Path) {
    write_save_file(
        path,
        &[SaveEntry {
            pv_name: "T:SAVED".into(),
            value: "1".into(),
            connected: true,
        }],
    )
    .await
    .unwrap();
}

/// SR_STATUS_INIT: a list that has not saved yet (save_restore.c:2441).
#[epics_macros_rs::epics_test]
async fn a_set_that_has_not_saved_reports_no_status() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path().join("s.sav"), backup(false, 0)).await;
    assert_eq!(rig.next_status().await, NO_STATUS);
    assert_eq!(rig.next_list().await, NO_STATUS);
    rig.stop().await;
}

/// SR_STATUS_OK: a clean save (save_restore.c:2012).
#[epics_macros_rs::epics_test]
async fn a_clean_save_reports_ok() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path().join("s.sav"), backup(false, 0)).await;
    assert_eq!(rig.save().await, OK);
    rig.stop().await;
}

/// SR_STATUS_WARN: the `.savB` was missing and written anew
/// (save_restore.c:2074).
#[epics_macros_rs::epics_test]
async fn a_rewritten_savb_reports_warning() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path().join("s.sav"), backup(true, 0)).await;
    assert_eq!(rig.save().await, WARNING);
    rig.stop().await;
}

/// SR_STATUS_FAIL: the `.sav` cannot be written (save_restore.c:2101).
#[epics_macros_rs::epics_test]
async fn an_unwritable_sav_reports_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path().join("absent").join("s.sav"), backup(false, 0)).await;
    assert_eq!(rig.save().await, FAILURE);
    rig.stop().await;
}

/// SR_STATUS_SEQ_WARN: a sequence file cannot be written
/// (save_restore.c:2197).
#[epics_macros_rs::epics_test]
async fn an_unwritable_sequence_file_reports_seq_fail() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("s.sav");
    valid_sav(&sav).await;
    std::fs::create_dir(sav.with_extension("sav0")).unwrap();
    let mut rig = Rig::new(sav, backup(false, 1)).await;
    assert_eq!(rig.save().await, SEQ_FAIL);
    rig.stop().await;
}

/// SR_status is the worst list status, not the last one saved
/// (save_restore.c:1044); INIT is the lowest code, so a list that has not
/// saved holds it at No Status.
#[epics_macros_rs::epics_test]
async fn sr_status_is_the_worst_list_status() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::with_sets(vec![
        (
            "bad",
            dir.path().join("absent").join("bad.sav"),
            backup(false, 0),
        ),
        ("good", dir.path().join("good.sav"), backup(false, 0)),
    ])
    .await;
    assert_eq!(rig.next_status().await, NO_STATUS);
    rig.mgr.manual_save("good", &rig.db).await.unwrap();
    sleep(Duration::from_millis(1500)).await;
    assert!(
        rig.status.try_recv().is_err(),
        "the unsaved list holds No Status"
    );
    let _ = rig.mgr.manual_save("bad", &rig.db).await;
    assert_eq!(rig.next_status().await, FAILURE);
    rig.mgr.manual_save("good", &rig.db).await.unwrap();
    sleep(Duration::from_millis(1500)).await;
    assert!(
        rig.status.try_recv().is_err(),
        "a clean save of the other list leaves Failure"
    );
    rig.stop().await;
}
