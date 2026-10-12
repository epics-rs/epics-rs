//! SR_statusStr is the worst list's status text and SR_recentlyStr the
//! newest event, both written once per reporting pass
//! (save_restore.c:1043-1046, :1063-1065). softIoc R7.0.10 on
//! save_restoreStatus.db (P=T:), `caput` standing in for autosave's `ca_put`:
//!
//! ```text
//! $ camonitor -t n T:SR_statusStr
//! T:SR_statusStr                  Status unknown UDF NO_ALARM
//! $ caput -t T:SR_statusStr Ok
//! T:SR_statusStr                  Ok
//! ```
//!
//! The texts are C's: "Initializing list" (:2442), "Ok" (:904, :2013),
//! "Can't write .sav file" (:2102), "list '%s' enabled" (:1485),
//! "Wrote '%s'" (:2134), "Can't write '%s'" (:2104), "Manual restore
//! succeeded" (:3261), "Manual restore failed" (:3284).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::{Reactor, TaskHandle, sleep};
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::{AutosaveBuilder, AutosaveManager};
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// save_restoreStatus.db's two global strings, verbatim.
const DB: &str = r#"
record(stringout, "T:SR_recentlyStr") {
  field(DTYP, "Soft Channel")
  field(VAL, "Status unknown")
}
record(stringout, "T:SR_statusStr") {
  field(DTYP, "Soft Channel")
  field(VAL, "Status unknown")
}
record(ao, "T:SAVED") { }
"#;

struct Rig {
    db: Arc<PvDatabase>,
    mgr: Arc<AutosaveManager>,
    handle: Option<TaskHandle<()>>,
    status: EventReader,
    recently: EventReader,
}

impl Rig {
    async fn new(save_path: PathBuf) -> Self {
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
                    DbFieldType::String,
                    (EventMask::VALUE | EventMask::ALARM).bits(),
                )
                .unwrap()
        };
        let status = sub("T:SR_statusStr");
        let recently = sub("T:SR_recentlyStr");
        let mgr = Arc::new(
            AutosaveBuilder::new()
                .status_prefix("T:")
                .add_set(SaveSetConfig {
                    name: "s".into(),
                    save_path,
                    strategy: SaveStrategy::Manual,
                    request_file: None,
                    request_pvs: vec!["T:SAVED".into()],
                    backup: BackupConfig {
                        enable_savb: false,
                        num_seq_files: 0,
                        seq_period: Duration::from_secs(60),
                        enable_dated: false,
                        dated_interval: Duration::from_secs(3600),
                    },
                    macros: HashMap::new(),
                    search_paths: Vec::new(),
                })
                .build()
                .await,
        );
        let handle = Some(mgr.clone().start(&Reactor::current().unwrap(), db.clone()));
        Self {
            db,
            mgr,
            handle,
            status,
            recently,
        }
    }

    /// The (SR_statusStr, SR_recentlyStr) posts of the next pass.
    async fn pass(&mut self) -> (Vec<String>, Vec<String>) {
        sleep(Duration::from_millis(1200)).await;
        (
            texts(&self.db, "T:SR_statusStr", &mut self.status),
            texts(&self.db, "T:SR_recentlyStr", &mut self.recently),
        )
    }

    async fn stop(mut self) {
        self.mgr.shutdown();
        let _ = self.handle.take().unwrap().await;
    }
}

fn texts(db: &PvDatabase, name: &str, rx: &mut EventReader) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let EpicsValue::String(s) = e
            .delivered_value(&db.get_record(name).unwrap().read(), "VAL")
            .expect("VAL reads")
        {
            out.push(s.as_str_lossy().into_owned());
        }
    }
    out
}

fn one(t: &str) -> Vec<String> {
    vec![t.to_string()]
}

#[epics_macros_rs::epics_test]
async fn a_clean_save_reports_ok_and_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path().join("s.sav")).await;
    assert_eq!(
        rig.pass().await,
        (one("Initializing list"), one("list 's.sav' enabled"))
    );
    rig.mgr.manual_save("s", &rig.db).await.unwrap();
    assert_eq!(rig.pass().await, (one("Ok"), one("Wrote 's.sav'")));
    rig.stop().await;
}

#[epics_macros_rs::epics_test]
async fn a_failed_save_reports_its_text_and_cant_write() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path().join("absent").join("s.sav")).await;
    rig.pass().await;
    let _ = rig.mgr.manual_save("s", &rig.db).await;
    assert_eq!(
        rig.pass().await,
        (one("Can't write .sav file"), one("Can't write 's.sav'"))
    );
    rig.stop().await;
}

#[epics_macros_rs::epics_test]
async fn a_manual_restore_reports_its_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::new(dir.path().join("s.sav")).await;
    rig.pass().await;
    let _ = rig.mgr.manual_restore("s", &rig.db).await;
    assert_eq!(
        rig.pass().await.1,
        one("Manual restore failed"),
        "no file yet"
    );
    rig.mgr.manual_save("s", &rig.db).await.unwrap();
    rig.mgr.manual_restore("s", &rig.db).await.unwrap();
    assert_eq!(rig.pass().await.1, one("Manual restore succeeded"));
    rig.stop().await;
}
