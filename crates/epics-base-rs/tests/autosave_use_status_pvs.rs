//! C `save_restoreUseStatusPVs`: the general status PVs are connected only
//! if it is set when the save task starts (save_restore.c:813), and the
//! lists' own PVs are written on each pass it is set (:1067).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::autosave::settings::SrSettings;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringout, "T:SR_statusStr") { field(VAL, "x") }
record(stringout, "T:SR_rebootStatusStr") { field(VAL, "x") }
record(stringout, "T:SR_0_Name") { field(VAL, "Not In Use") }
record(ao, "T:SAVED") { }
"#;

fn text(db: &PvDatabase, name: &str) -> String {
    match db.get_pv(name).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => panic!("{v:?}"),
    }
}

async fn put(db: &PvDatabase, name: &str, v: &str) {
    db.put_pv(name, EpicsValue::String(v.into())).await.unwrap();
}

/// (SR_rebootStatusStr, SR_statusStr, SR_0_Name) after the task's start
/// with UseStatusPVs `at_start`, then the same after `x` is put back and
/// UseStatusPVs is `later`.
async fn run(at_start: i32, later: i32) -> [(String, String, String); 2] {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let dir = tempfile::tempdir().unwrap();
    let settings = Arc::new(SrSettings::default());
    settings.set_use_status_pvs(at_start);
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .status_prefix("T:")
            .settings(settings.clone())
            .add_set(
                SaveSetConfig {
                    name: "s".into(),
                    save_path: dir.path().join("s.sav"),
                    request_file: None,
                    request_pvs: vec!["T:SAVED".into()],
                    backup: BackupConfig { enable_savb: false },
                    macros: HashMap::new(),
                    macro_string: None,
                    search_paths: Vec::new(),
                },
                SaveStrategy::Manual,
            )
            .build()
            .await,
    );
    let handle = mgr.clone().start(db.clone()).unwrap();
    let read = || {
        (
            text(&db, "T:SR_rebootStatusStr"),
            text(&db, "T:SR_statusStr"),
            text(&db, "T:SR_0_Name"),
        )
    };
    sleep(Duration::from_millis(500)).await;
    let first = read();
    settings.set_use_status_pvs(later);
    for name in ["T:SR_rebootStatusStr", "T:SR_statusStr", "T:SR_0_Name"] {
        put(&db, name, "x").await;
    }
    sleep(Duration::from_millis(1500)).await;
    let second = read();
    mgr.shutdown();
    let _ = handle.await;
    [first, second]
}

fn row(a: &str, b: &str, c: &str) -> (String, String, String) {
    (a.into(), b.into(), c.into())
}

#[epics_macros_rs::epics_test]
async fn set_throughout_writes_both() {
    assert_eq!(
        run(1, 1).await,
        [
            row("Ok", "Enabling list...", "s"),
            row("x", "Enabling list...", "s")
        ]
    );
}

#[epics_macros_rs::epics_test]
async fn cleared_at_start_writes_neither() {
    assert_eq!(
        run(0, 0).await,
        [row("x", "x", "Not In Use"), row("x", "x", "x")]
    );
}

/// The general PVs stay connected; the lists' stop.
#[epics_macros_rs::epics_test]
async fn cleared_later_stops_only_the_lists() {
    assert_eq!(
        run(1, 0).await,
        [
            row("Ok", "Enabling list...", "s"),
            row("x", "Enabling list...", "x")
        ]
    );
}

/// The lists' PVs start; the general ones were never connected.
#[epics_macros_rs::epics_test]
async fn set_later_starts_only_the_lists() {
    assert_eq!(
        run(0, 1).await,
        [row("x", "x", "Not In Use"), row("x", "x", "s")]
    );
}
