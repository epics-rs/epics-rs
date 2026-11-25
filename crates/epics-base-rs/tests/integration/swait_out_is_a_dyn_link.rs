//! swait's OUTN is a `recDynLink` output: `execOutput` puts through
//! `recDynLinkPutCallback` (`swaitRecord.c:777-788`), a CA put, so the
//! target is processed as `dbPutField` processes a Passive record, and a
//! name with a link modifier names no PV.
//!
//! oracleSwaitIoc (calc R3-7-5), each swait processed once with `caput -c`,
//! targets read as `VAL SEVR UDF`:
//!
//! ```text
//! SW:O OUTN SQ:T                 SQ:T  5 NO_ALARM 0
//! SW:D DOPT Use DOL, DOLN SQ:SRC SQ:T2 7 NO_ALARM 0
//! SW:F OUTN SQ:T3, FLNK SQ:CNT   SQ:T3 3 NO_ALARM 0   SQ:CNT 1
//! SW:V OUTN SQ:T4.VAL            SQ:T4 4 NO_ALARM 0
//! SW:L OUTN SQ:L (longout)       SQ:L  6 NO_ALARM 0
//! SW:P SCAN 1 second             SQ:T5 8 NO_ALARM 0
//! PV:W OUTN "SQ:T6 PP"           OUTV PV BAD          SQ:T6 0
//! ```

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SQ:T") { }
record(ao, "SQ:T2") { }
record(ao, "SQ:T3") { field(FLNK, "SQ:CNT") }
record(calc, "SQ:CNT") { field(CALC, "A+1") field(INPA, "SQ:CNT") }
record(ao, "SQ:T4") { }
record(ao, "SQ:T5") { }
record(ao, "SQ:T6") { }
record(longout, "SQ:L") { }
record(ao, "SQ:SRC") { field(VAL, "7") field(PINI, "YES") }
record(swait, "SW:O") { field(CALC, "5") field(OUTN, "SQ:T") field(OOPT, "Every Time") }
record(swait, "SW:D") { field(DOPT, "Use DOL") field(DOLN, "SQ:SRC") field(OUTN, "SQ:T2") field(CALC, "1") field(OOPT, "Every Time") }
record(swait, "SW:F") { field(CALC, "3") field(OUTN, "SQ:T3") field(OOPT, "Every Time") }
record(swait, "SW:V") { field(CALC, "4") field(OUTN, "SQ:T4.VAL") field(OOPT, "Every Time") }
record(swait, "SW:L") { field(CALC, "6") field(OUTN, "SQ:L") field(OOPT, "Every Time") }
record(swait, "SW:P") { field(SCAN, "1 second") field(CALC, "8") field(OUTN, "SQ:T5") field(OOPT, "Every Time") }
record(swait, "PV:W") { field(CALC, "1") field(OUTN, "SQ:T6 PP") field(OOPT, "Every Time") }
"#;

const PV_OK: f64 = 0.0;
const PV_NC: f64 = 1.0;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// `(VAL, SEVR, UDF)` of an output target.
fn target(db: &PvDatabase, rec: &str) -> (f64, f64, f64) {
    (
        num(db, &format!("{rec}.VAL")),
        num(db, &format!("{rec}.SEVR")),
        num(db, &format!("{rec}.UDF")),
    )
}

/// Process `rec` until `done`: DOL's value lands on the link's first
/// monitor, after DOLV reads PV OK, and shows only on OUT.
async fn process_until(db: &PvDatabase, rec: &str, done: impl Fn(&PvDatabase) -> bool) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        db.put_record_field_from_ca(rec, "PROC", EpicsValue::String("1".into()))
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        if done(db) {
            return;
        }
    }
}

async fn settle(db: &PvDatabase, done: impl Fn(&PvDatabase) -> bool) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if done(db) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

#[epics_macros_rs::epics_test]
async fn the_output_is_a_ca_put_that_processes_its_target() {
    let ioc = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = std::sync::Arc::clone(ioc.database());
    assert!(ioc.run().await, "SW:P scans");
    settle(&db, |db| {
        num(db, "SW:D.DOLV") == PV_OK && num(db, "SW:L.OUTV") == PV_OK
    })
    .await;
    for w in ["SW:O", "SW:F", "SW:V", "SW:L", "PV:W"] {
        db.put_record_field_from_ca(w, "PROC", EpicsValue::String("1".into()))
            .await
            .unwrap();
    }
    process_until(&db, "SW:D", |db| num(db, "SQ:T2") == 7.0).await;
    settle(&db, |db| {
        num(db, "SQ:CNT") == 1.0
            && ["SQ:T", "SQ:T2", "SQ:T4", "SQ:T5", "SQ:L"]
                .iter()
                .all(|t| num(db, &format!("{t}.UDF")) == 0.0)
    })
    .await;
    let done = |v| (v, 0.0, 0.0);
    assert_eq!(
        [
            target(&db, "SQ:T"),
            target(&db, "SQ:T2"),
            target(&db, "SQ:T3"),
            target(&db, "SQ:T4"),
            target(&db, "SQ:L"),
            target(&db, "SQ:T5"),
        ],
        [
            done(5.0),
            done(7.0),
            done(3.0),
            done(4.0),
            done(6.0),
            done(8.0)
        ],
        "(VAL, SEVR, UDF) of SQ:T SQ:T2 SQ:T3 SQ:T4 SQ:L SQ:T5"
    );
    assert_eq!(
        (num(&db, "SQ:CNT"), num(&db, "SW:D.DOLD")),
        (1.0, 7.0),
        "SQ:T3's FLNK, SW:D's DOLD"
    );
    assert_eq!(
        (num(&db, "PV:W.OUTV"), target(&db, "SQ:T6")),
        (PV_NC, (0.0, 3.0, 1.0)),
        "\"SQ:T6 PP\" names no PV"
    );
}
