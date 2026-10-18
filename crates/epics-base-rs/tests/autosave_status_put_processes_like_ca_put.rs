//! C autosave writes its status PVs with `ca_put` (save_restore.c:1061-1062),
//! which rsrv serves as `dbPutField`, so the records process. No oracle IOC
//! loads autosave; this is softIoc R7.0.10 on autosave's own
//! save_restoreStatus.db (P=T:), with `caput` standing in for autosave's
//! `ca_put` of 2 into SR_status and of the first heartbeat:
//!
//! ```text
//! $ camonitor -t sI T:SR_status T:SR_heartbeat T:SR_i_am_alive
//! $ caput -t T:SR_status 2; caput -t T:SR_heartbeat 1
//! T:SR_status       +1160542235.784075 Warning STATE MINOR
//! T:SR_heartbeat    +1160542235.822730 1
//! T:SR_i_am_alive   +1160542235.822743 1
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::{Reactor, sleep};
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_file::{SaveEntry, write_save_file};
use epics_base_rs::server::autosave::save_set::{ManualSource, SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// save_restoreStatus.db's heartbeat chain and SR_status, verbatim.
const DB: &str = r#"
record(bo, "T:SR_heartbeat") {
  field(DTYP, "Soft Channel")
  field(ZNAM, "0")
  field(ONAM, "1")
  field(FLNK, "T:SR_i_am_alive")
}
record(bo, "T:SR_i_am_alive") {
  field(DTYP, "Soft Channel")
  field(DOL, "1")
  field(UDF, "0")
  field(ZNAM, "0")
  field(ONAM, "1")
  field(OUT, "T:SR_deadIfZero.VAL PP")
}
record(bo, "T:SR_deadIfZero") {
  field(DTYP, "Soft Channel")
  field(ZNAM, "0")
  field(ONAM, "1")
  field(HIGH, "300")
}
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
"#;

fn set_config(save_path: PathBuf) -> SaveSetConfig {
    SaveSetConfig {
        name: "s".into(),
        save_path,
        request_file: None,
        request_pvs: vec!["T:SR_status".into()],
        backup: BackupConfig { enable_savb: false },
        macros: HashMap::new(),
        macro_string: String::new(),
        search_paths: Vec::new(),
    }
}

async fn first_post(db: &PvDatabase, name: &str, rx: &mut EventReader) -> (EpicsValue, u16, u16) {
    for _ in 0..500 {
        if let Ok(e) = rx.try_recv() {
            return (
                e.delivered_value(&db.get_record(name).unwrap().read(), "VAL")
                    .expect("VAL reads"),
                e.stat(),
                e.sevr(),
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("no post within 5 s");
}

/// The heartbeat toggles once per reporting pass, `(SR_heartbeat + 1) % 2`
/// (save_restore.c:1060). softIoc on the same records:
///
/// ```text
/// $ for v in 1 0 1; do caput -t T:SR_heartbeat $v; done
/// T:SR_heartbeat                  1
/// T:SR_i_am_alive                 1
/// T:SR_heartbeat                  0
/// T:SR_heartbeat                  1
/// ```
#[epics_macros_rs::epics_test]
async fn three_heartbeats_toggle_the_bo_as_ca_put_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let db: Arc<PvDatabase> = db;
    let mut subs: Vec<EventReader> = ["T:SR_heartbeat", "T:SR_i_am_alive"]
        .iter()
        .enumerate()
        .map(|(sid, name)| {
            db.get_record(name)
                .unwrap()
                .write()
                .add_subscriber(
                    "VAL",
                    sid as u32 + 1,
                    DbFieldType::Enum,
                    (EventMask::VALUE | EventMask::ALARM).bits(),
                )
                .unwrap()
        })
        .collect();

    let dir = tempfile::tempdir().unwrap();
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .status_prefix("T:")
            .add_set(set_config(dir.path().join("s.sav")), SaveStrategy::Manual)
            .build()
            .await,
    );
    let handle = mgr
        .clone()
        .start(
            &Reactor::current().expect("the test driver enters an executor"),
            db.clone(),
        )
        .unwrap();

    let mut beats = Vec::new();
    for _ in 0..3 {
        beats.push(first_post(&db, "T:SR_heartbeat", &mut subs[0]).await);
    }
    mgr.shutdown();
    let _ = handle.await;
    let mut alive = Vec::new();
    while let Ok(e) = subs[1].try_recv() {
        alive.push((
            e.delivered_value(&db.get_record("T:SR_i_am_alive").unwrap().read(), "VAL")
                .expect("VAL reads"),
            e.sevr(),
        ));
    }

    assert_eq!(
        beats,
        [
            (EpicsValue::Enum(1), 0, 0),
            (EpicsValue::Enum(0), 0, 0),
            (EpicsValue::Enum(1), 0, 0)
        ]
    );
    assert_eq!(alive, [(EpicsValue::Enum(1), 0)]);
}

/// `do_manual_restore` writes each saved value with `ca_put` too
/// (save_restore.c:3245), so restoring 2 into SR_status is the same `caput`.
#[epics_macros_rs::epics_test]
async fn a_manual_restore_processes_as_ca_put_does() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut rx = db
        .get_record("T:SR_status")
        .unwrap()
        .write()
        .add_subscriber(
            "VAL",
            1,
            DbFieldType::Enum,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sav");
    write_save_file(
        &path,
        &[SaveEntry {
            pv_name: "T:SR_status".into(),
            value: "2".into(),
            connected: true,
        }],
    )
    .await
    .unwrap();
    let mgr = AutosaveBuilder::new()
        .add_set(set_config(path.clone()), SaveStrategy::Manual)
        .build()
        .await;

    let status = mgr
        .manual_restore(&db, &path, "s.sav", ManualSource::SaveFile, "")
        .await;
    assert_eq!(status, 0);
    let (value, stat, sevr) = first_post(&db, "T:SR_status", &mut rx).await;
    assert_eq!(
        (value.to_f64(), stat, sevr),
        (Some(2.0), 7, 1),
        "Warning STATE MINOR"
    );
}
