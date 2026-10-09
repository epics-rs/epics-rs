//! `recGblGetTimeStampSimm` (`recGbl.c:313-343`) takes the SIOL's stamp only
//! after `TSEL` has left `TSE` at `epicsTimeEventDeviceTime` and was not a
//! `.TIME` link; every other `TSEL`/`TSE` ends before the simulation arm.
//!
//! Measured on `R7.0.10-146-g8f5015b66` softIoc (SIMM=1, SIOL=OTHER, SRC and
//! OTHER processed a second apart, then each case once):
//!   * TSE=-2, TSEL=SS_SRC.TIME   -> SRC's stamp (a .TIME TSEL returns first)
//!   * TSEL=TSEM2 (VAL -2)        -> OTHER's stamp
//!   * TSEL=TSE0  (VAL 0)         -> the clock
//!   * TSE=-2, no TSEL            -> OTHER's stamp
//!   * TSE=0, no TSEL             -> the clock
//!   * TSE=-2, constant SIOL      -> the clock
//!
//! A SIOL that names nothing logs
//! `recGblGetTimeStampSimm: dbGetTimeStamp (sim mode) failed, <rec>.SIOL = <pv>`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

const SRC: Duration = Duration::new(3_000_000, 111_000_000);
const OTHER: Duration = Duration::new(3_000_001, 222_000_000);

const DB_TEXT: &str = r#"
record(ai, "SS_SRC") {}
record(ai, "SS_OTHER") {}
record(longout, "SS_TSEM2") { field(VAL, "-2") }
record(longout, "SS_TSE0") { field(VAL, "0") }
record(ai, "SS_TIME")  { field(SIMM, "1") field(TSE, "-2") field(TSEL, "SS_SRC.TIME") field(SIOL, "SS_OTHER") }
record(ai, "SS_VIA2")  { field(SIMM, "1") field(TSEL, "SS_TSEM2") field(SIOL, "SS_OTHER") }
record(ai, "SS_VIA0")  { field(SIMM, "1") field(TSEL, "SS_TSE0") field(SIOL, "SS_OTHER") }
record(ai, "SS_OWN")   { field(SIMM, "1") field(TSE, "-2") field(SIOL, "SS_OTHER") }
record(ai, "SS_OWN0")  { field(SIMM, "1") field(TSE, "0") field(SIOL, "SS_OTHER") }
record(ai, "SS_CONST") { field(SIMM, "1") field(TSE, "-2") }
record(ai, "SS_DEAD")  { field(SIMM, "1") field(TSE, "-2") field(SIOL, "NOSUCH:Z") }
record(ai, "SS_DEAD0") { field(SIMM, "1") field(TSE, "0") field(SIOL, "NOSUCH:Z") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn stamp_after_processing(db: &PvDatabase, rec: &str) -> SystemTime {
    for (name, d) in [("SS_SRC", SRC), ("SS_OTHER", OTHER)] {
        db.get_record(name).unwrap().write().common.time = SystemTime::UNIX_EPOCH + d;
    }
    let cell = db.get_record(rec).unwrap();
    cell.write().common.time = SystemTime::UNIX_EPOCH;
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    cell.read().common.time
}

#[epics_macros_rs::epics_test]
async fn the_siol_stamp_is_taken_only_at_device_time_and_not_under_a_time_tsel() {
    let db = ioc().await;
    let before = SystemTime::now();
    let at = |d: Duration| SystemTime::UNIX_EPOCH + d;
    let mut bad = Vec::new();
    for (rec, want) in [
        ("SS_TIME", Some(at(SRC))),
        ("SS_VIA2", Some(at(OTHER))),
        ("SS_OWN", Some(at(OTHER))),
        ("SS_VIA0", None),
        ("SS_OWN0", None),
        ("SS_CONST", None),
    ] {
        let got = stamp_after_processing(&db, rec).await;
        let ok = match want {
            Some(w) => got == w,
            None => got >= before && got <= SystemTime::now(),
        };
        if !ok {
            bad.push(format!("{rec}: got {got:?}, want {want:?} (None = clock)"));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

#[epics_macros_rs::epics_test]
async fn a_siol_that_gives_no_stamp_is_logged_only_when_the_stamp_is_owed() {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().expect("sink").push(m.to_string());
    });
    let db = ioc().await;
    stamp_after_processing(&db, "SS_DEAD0").await;
    epics_base_rs::runtime::log::errlog_flush();
    let log = heard.lock().expect("sink").join("\n");
    assert!(
        !log.contains("SS_DEAD0.SIOL"),
        "TSE=0 owes no SIOL stamp: {log}"
    );
    stamp_after_processing(&db, "SS_DEAD").await;
    epics_base_rs::runtime::log::errlog_flush();
    let log = heard.lock().expect("sink").join("\n");
    assert!(
        log.contains(
            "recGblGetTimeStampSimm: dbGetTimeStamp (sim mode) failed, SS_DEAD.SIOL = NOSUCH:Z"
        ),
        "{log}"
    );
}
