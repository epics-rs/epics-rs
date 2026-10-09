//! `recGblGetTimeStampSimm(prec, prec->simm, NULL)` (`recGbl.c:331-342`): an
//! output record with `TSE = -2` and a non-NO `SIMM` has no SIOL to take a
//! stamp from and stamps the current time instead of leaving `TIME` alone.
//!
//! Measured on `R7.0.10-146-g8f5015b66` softIoc, `TSE=-2`, one `.PROC`: ao,
//! bo, longout, stringout, lso, aao and int64out under `SIMM=1` carry the
//! wall clock; the same ao with `SIMM=0` keeps `<undefined>`; mbbo and
//! mbboDirect stay `<undefined>` under `SIMM=1` too, because they `goto
//! CONTINUE` past the stamp while `UDF` is set (`mbboRecord.c:210`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use epics_base_rs::server::ioc_builder::IocBuilder;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

const STALE: Duration = Duration::new(3_000_000, 333_333_333);

const DB_TEXT: &str = r#"
record(ao, "ST_AO")        { field(SIMM, "1") field(TSE, "-2") }
record(bo, "ST_BO")        { field(SIMM, "1") field(TSE, "-2") }
record(longout, "ST_LO")   { field(SIMM, "1") field(TSE, "-2") }
record(stringout, "ST_SO") { field(SIMM, "1") field(TSE, "-2") }
record(lso, "ST_LSO")      { field(SIMM, "1") field(TSE, "-2") field(SIZV, "20") }
record(aao, "ST_AAO")      { field(SIMM, "1") field(TSE, "-2") field(NELM, "2") field(FTVL, "LONG") }
record(int64out, "ST_I64") { field(SIMM, "1") field(TSE, "-2") }
record(mbbo, "ST_MBBO")    { field(SIMM, "1") field(TSE, "-2") }
record(mbboDirect, "ST_MBD") { field(SIMM, "1") field(TSE, "-2") }
record(ao, "ST_NOSIM")     { field(SIMM, "0") field(TSE, "-2") }
"#;

async fn process_from_stale(db: &Db, rec: &str) -> SystemTime {
    let cell = db.get_record(rec).expect("record exists");
    cell.write().common.time = SystemTime::UNIX_EPOCH + STALE;
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    cell.read().common.time
}

#[epics_macros_rs::epics_test]
async fn a_simulated_output_with_device_time_stamps_the_clock() {
    let db: Db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let before = SystemTime::now();
    let mut bad = Vec::new();
    for rec in [
        "ST_AO", "ST_BO", "ST_LO", "ST_SO", "ST_LSO", "ST_AAO", "ST_I64",
    ] {
        let t = process_from_stale(&db, rec).await;
        if t < before || t > SystemTime::now() {
            bad.push(format!("{rec}: {t:?}"));
        }
    }
    assert!(bad.is_empty(), "not stamped with the current time: {bad:?}");
}

#[epics_macros_rs::epics_test]
async fn device_time_is_left_alone_without_simulation_or_with_udf_set() {
    let db: Db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let stale = SystemTime::UNIX_EPOCH + STALE;
    let mut bad = Vec::new();
    for rec in ["ST_NOSIM", "ST_MBBO", "ST_MBD"] {
        let t = process_from_stale(&db, rec).await;
        if t != stale {
            bad.push(format!("{rec}: {t:?}"));
        }
    }
    assert!(bad.is_empty(), "stamp was rewritten: {bad:?}");
}
