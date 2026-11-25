//! Each of the first eight lists, in creation order, reports through
//! save_restoreStatus.db's SR_<i>_Status/_Name/_State/_StatusStr/_Time
//! (save_restore.c:1109-1121, :2466-2470). softIoc R7.0.10 on that db
//! (P=T:), `caput` standing in for autosave's `ca_put`:
//!
//! ```text
//! $ camonitor -t n T:SR_0_Name T:SR_0_State T:SR_0_StatusStr T:SR_0_Time
//! T:SR_0_Name                     Not In Use UDF NO_ALARM
//! T:SR_0_State                    0 UDF INVALID
//! T:SR_0_StatusStr                Status unknown UDF NO_ALARM
//! T:SR_0_Time                     Not yet saved UDF NO_ALARM
//! $ caput -t T:SR_0_Name s; caput -t T:SR_0_State 0; caput -t T:SR_0_StatusStr Ok
//! $ caput -t T:SR_0_Time "Sun Oct 11 14:00:00"
//! T:SR_0_Name                     s
//! T:SR_0_State                    0
//! T:SR_0_StatusStr                Ok
//! T:SR_0_Time                     Sun Oct 11 14:00:00
//! ```
//!
//! The texts are C's: "Enabling list..." (:1431, the first pass), "Ok" (:2013),
//! "%d %s not saved" (:2130), and Time is `TIMEFMT_noY` (:214) of the last
//! save, written only from SR_STATUS_WARN up (:1118).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// save_restoreStatus.db's SR_0 strings and state, and SR_1_Name, verbatim.
const DB: &str = r#"
record(stringout, "T:SR_0_Name") {
  field(VAL, "Not In Use")
  field(DTYP, "Soft Channel")
}
record(longout, "T:SR_0_State") {
  field(DESC, "Save method for pass 0")
  field(DTYP, "Soft Channel")
}
record(stringout, "T:SR_0_StatusStr") {
  field(DTYP, "Soft Channel")
  field(VAL, "Status unknown")
}
record(stringout, "T:SR_0_Time") {
  field(DTYP, "Soft Channel")
  field(VAL, "Not yet saved")
}
record(stringout, "T:SR_1_Name") {
  field(VAL, "Not In Use")
  field(DTYP, "Soft Channel")
}
record(ao, "T:SAVED") { }
"#;

fn set(name: &str, save_path: PathBuf, pv: &str) -> SaveSetConfig {
    SaveSetConfig {
        name: name.into(),
        save_path,
        request_file: None,
        request_pvs: vec![pv.into()],
        backup: BackupConfig { enable_savb: false },
        macros: HashMap::new(),
        macro_string: None,
        search_paths: Vec::new(),
    }
}

fn subscribe(db: &PvDatabase, name: &str) -> EventReader {
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
}

/// Every post `rx` has seen, as (text, SEVR).
fn posts(db: &PvDatabase, name: &str, rx: &mut EventReader) -> Vec<(String, u16)> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        let text = match e
            .delivered_value(&db.get_record(name).unwrap().read(), "VAL")
            .expect("VAL reads")
        {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            v => format!("{}", v.to_f64().unwrap()),
        };
        out.push((text, e.sevr()));
    }
    out
}

/// Add each reader's posts to its `seen` until `done` holds of them, or the fact
/// budget runs out: the save task reports on its own one-second passes, later under
/// load. The caller's assertion names what was missing.
async fn until(
    db: &PvDatabase,
    names: &[&str],
    rx: &mut [EventReader],
    seen: &mut [Vec<(String, u16)>],
    done: impl Fn(&[Vec<(String, u16)>]) -> bool,
) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    loop {
        for ((name, rx), seen) in names.iter().zip(rx.iter_mut()).zip(seen.iter_mut()) {
            seen.extend(posts(db, name, rx));
        }
        if done(seen) || Instant::now() >= deadline {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

#[epics_macros_rs::epics_test]
async fn a_clean_save_fills_its_list_records_in_creation_order() {
    let db = build().await;
    let names = [
        "T:SR_0_Name",
        "T:SR_0_State",
        "T:SR_0_StatusStr",
        "T:SR_0_Time",
        "T:SR_1_Name",
    ];
    let mut rx: Vec<_> = names.iter().map(|n| subscribe(&db, n)).collect();
    let dir = tempfile::tempdir().unwrap();
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .status_prefix("T:")
            .add_set(
                set("first", dir.path().join("s.sav"), "T:SAVED"),
                SaveStrategy::Manual,
            )
            .add_set(
                set("second", dir.path().join("t.sav"), "T:SAVED"),
                SaveStrategy::Manual,
            )
            .build()
            .await,
    );
    let handle = mgr.clone().start(db.clone()).unwrap();
    let one = |t: &str| vec![(t.to_string(), 0u16)];
    let want = [
        one("s"),
        one("0"),
        one("Enabling list..."),
        vec![],
        one("t"),
    ];
    let mut before = vec![Vec::new(); names.len()];
    until(&db, &names, &mut rx, &mut before, |seen| seen == want).await;
    assert_eq!(
        before, want,
        "INIT is below SR_STATUS_WARN, so Time keeps its db value"
    );
    // The list is enabled, so the save's status is not overwritten by it,
    // as C's task saves only after `enable_list`.
    mgr.manual_save("first", &db).await.unwrap();
    let saved = mgr.sets()[0]
        .0
        .save_time()
        .format("%a %b %d %H:%M:%S")
        .to_string();
    let want = [vec![], vec![], one("Ok"), one(&saved), vec![]];
    let mut after = vec![Vec::new(); names.len()];
    until(&db, &names, &mut rx, &mut after, |seen| seen == want).await;
    // Whatever a later pass posts is read once the task has stopped.
    mgr.shutdown();
    let _ = handle.await;
    until(&db, &names, &mut rx, &mut after, |_| true).await;
    assert_eq!(after, want, "a stringout posts only a changed value");
}

#[epics_macros_rs::epics_test]
async fn a_value_it_could_not_fetch_is_counted_in_statusstr() {
    let db = build().await;
    let mut rx = subscribe(&db, "T:SR_0_StatusStr");
    let dir = tempfile::tempdir().unwrap();
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .status_prefix("T:")
            .add_set(
                set("first", dir.path().join("s.sav"), "T:ABSENT"),
                SaveStrategy::Manual,
            )
            .build()
            .await,
    );
    let handle = mgr.clone().start(db.clone()).unwrap();
    let names = ["T:SR_0_StatusStr"];
    let rx = std::slice::from_mut(&mut rx);
    let mut seen = vec![Vec::new()];
    let last_is = |text: &str| {
        let want = (text.to_string(), 0);
        move |seen: &[Vec<(String, u16)>]| seen[0].last() == Some(&want)
    };
    // As C's task saves only after `enable_list`, which would overwrite an
    // earlier save's status.
    until(&db, &names, rx, &mut seen, last_is("Enabling list...")).await;
    let _ = mgr.manual_save("first", &db).await;
    until(&db, &names, rx, &mut seen, last_is("1 value not saved")).await;
    mgr.shutdown();
    let _ = handle.await;
    until(&db, &names, rx, &mut seen, |_| true).await;
    assert_eq!(
        seen[0].last(),
        Some(&("1 value not saved".to_string(), 0)),
        "{seen:?}"
    );
}
