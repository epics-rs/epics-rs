//! C's disable support (save_restore.c:884-901): while `<prefix>SR_disable`
//! is nonzero the save task skips its passes, so no list saves and the
//! heartbeat stops, and once the disable is older than
//! `SR_disableMaxSecs` the task puts SR_disable back to 0. The C oracle
//! IOC, a monitor set of `A` under prefix `X:` with MaxSecs 4: the
//! heartbeat holds at 1 for the 2 s it is read, SR_disable reads Disable
//! after 3 s and Enable after 7 s, and `A 8`, put while disabled, is saved
//! only then.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_file::read_save_file;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// save_restoreStatus.db's heartbeat and disable records (P=X:), and `A`.
const DB: &str = r#"
record(bo, "X:SR_heartbeat") {
  field(DTYP, "Soft Channel")
  field(ZNAM, "0")
  field(ONAM, "1")
}
record(longout, "X:SR_disableMaxSecs") {
  field(DTYP, "Soft Channel")
}
record(bo, "X:SR_disable") {
  field(DTYP, "Soft Channel")
  field(ZNAM, "Enable")
  field(ONAM, "Disable")
}
record(ao, "A") {
  field(VAL, "7")
}
"#;

fn set_config(save_path: PathBuf) -> SaveSetConfig {
    SaveSetConfig {
        name: "s".into(),
        save_path,
        request_file: None,
        request_pvs: vec!["A".into()],
        backup: BackupConfig { enable_savb: false },
        macros: HashMap::new(),
        macro_string: None,
        search_paths: Vec::new(),
    }
}

fn read(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

async fn put(db: &PvDatabase, name: &str, v: i32) {
    db.put_pv(name, EpicsValue::Long(v)).await.unwrap();
}

async fn saved_a(path: &std::path::Path) -> Option<String> {
    let contents = read_save_file(path).await.ok()??;
    Some(contents.entries[0].value.clone())
}

#[epics_macros_rs::epics_test]
async fn a_disable_holds_the_passes_until_its_max_secs() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let db: Arc<PvDatabase> = db;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sav");
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .status_prefix("X:")
            .add_set(
                set_config(path.clone()),
                SaveStrategy::OnChange {
                    min_interval: Duration::from_secs(1),
                },
            )
            .build()
            .await,
    );
    let handle = mgr.clone().start(db.clone()).unwrap();
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while Instant::now() < deadline {
        if saved_a(&path).await.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(saved_a(&path).await.as_deref(), Some("7"));

    put(&db, "X:SR_disableMaxSecs", 2).await;
    put(&db, "X:SR_disable", 1).await;
    let disabled = Instant::now();
    sleep(Duration::from_millis(700)).await;
    let beat = read(&db, "X:SR_heartbeat");
    db.put_record_field_from_ca_no_notify("A", "VAL", EpicsValue::Double(8.0))
        .await
        .unwrap();
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(read(&db, "X:SR_heartbeat"), beat, "no pass while disabled");
    assert_eq!(read(&db, "X:SR_disable"), 1.0);
    assert_eq!(saved_a(&path).await.as_deref(), Some("7"), "no save");

    while read(&db, "X:SR_disable") != 0.0 {
        assert!(
            disabled.elapsed() < fact_budget::FACT_BUDGET,
            "never enabled"
        );
        sleep(Duration::from_millis(50)).await;
    }
    assert!(disabled.elapsed() > Duration::from_secs(2));
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while Instant::now() < deadline {
        if saved_a(&path).await.as_deref() == Some("8") {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(saved_a(&path).await.as_deref(), Some("8"));
    mgr.shutdown();
    let _ = handle.await;
}

/// A disable put back to 0 by hand restarts the time: C keeps the first
/// one's start (:888-897), and the oracle's next disable read Enable 2 s
/// later with MaxSecs 4, a C defect not kept.
#[epics_macros_rs::epics_test]
async fn a_new_disable_gets_its_own_max_secs() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let db: Arc<PvDatabase> = db;
    let dir = tempfile::tempdir().unwrap();
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .status_prefix("X:")
            .add_set(set_config(dir.path().join("s.sav")), SaveStrategy::Manual)
            .build()
            .await,
    );
    let handle = mgr.clone().start(db.clone()).unwrap();
    put(&db, "X:SR_disableMaxSecs", 2).await;
    put(&db, "X:SR_disable", 1).await;
    sleep(Duration::from_millis(1500)).await;
    put(&db, "X:SR_disable", 0).await;
    sleep(Duration::from_millis(2500)).await;
    put(&db, "X:SR_disable", 1).await;
    sleep(Duration::from_millis(1200)).await;
    assert_eq!(read(&db, "X:SR_disable"), 1.0);
    mgr.shutdown();
    let _ = handle.await;
}
