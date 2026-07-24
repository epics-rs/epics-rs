//! R17-64: EVERY process-time link read inherits an `MS` source's severity —
//! not just INP.
//!
//! C's `dbGetLink` on a DB link ends in `dbDbGetValue`'s tail
//! (`dbDbLink.c:228-232`):
//!
//! ```c
//! if (!status && precord != dbChannelRecord(chan))
//!     recGblInheritSevrMsg(plink->value.pv_link.pvlMask & pvlOptMsMode,
//!         plink->precord, ...->stat, ...->sevr, ...->amsg);
//! ```
//!
//! so it runs for whatever link the record just read: DOL (closed loop), SDIS,
//! TSEL, SELL, SIML, SIOL. The port had the inheritance owner but only the INP
//! path went through it; the others read with an alarm-dropping primitive.
//!
//! softIoc (EPICS 7.0.10), `SRC0` an ai driven to MAJOR:
//!
//! ```text
//! record(ao,"A1"){field(DOL,"SRC MS")  field(OMSL,"closed_loop")} -> MAJOR / LINK
//! record(bo,"B1"){field(DOL,"SRC MS")  field(OMSL,"closed_loop")} -> MAJOR / LINK
//! record(ai,"R2"){field(SIMM,"YES") field(SIOL,"SRC MS")}         -> MAJOR / LINK
//! record(ai,"R3"){field(SIML,"SRC0 MS")}                          -> MAJOR / LINK
//! record(ai,"D1"){field(SDIS,"SRC0 MS") field(DISV,"1")}          -> MAJOR / LINK
//! record(ai,"T2"){field(TSEL,"SRC0 MS")}                          -> MAJOR / LINK
//! record(ai,"R4"){field(SIML,"SRC0")}    (no MS)                  -> NO_ALARM
//! ```

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::types::EpicsValue;

/// A source record parked in MAJOR, the way `HIGH`/`HSV` parks one in C.
async fn db_with_major_source() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(5.0)))
        .await
        .unwrap();
    let rec = db.get_record("SRC").unwrap();
    let mut inst = rec.write();
    inst.common.sevr = AlarmSeverity::Major;
    inst.common.stat = alarm_status::HIGH_ALARM;
    db.clone()
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

async fn alarm(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

/// The closed-loop DOL read: `dbGetLink(&prec->dol, ...)` in `fetch_values`
/// runs the same tail as an INP read.
#[epics_macros_rs::epics_test]
async fn closed_loop_dol_inherits_ms() {
    let db = db_with_major_source().await;

    db.add_record("A1", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv("A1.DOL", EpicsValue::String("SRC MS".into()))
        .await
        .unwrap();
    db.put_pv("A1.OMSL", EpicsValue::String("closed_loop".into()))
        .await
        .unwrap();

    db.add_record("B1", Box::new(BoRecord::new(0)))
        .await
        .unwrap();
    db.put_pv("B1.DOL", EpicsValue::String("SRC MS".into()))
        .await
        .unwrap();
    db.put_pv("B1.OMSL", EpicsValue::String("closed_loop".into()))
        .await
        .unwrap();

    process(&db, "A1").await;
    process(&db, "B1").await;

    assert_eq!(
        alarm(&db, "A1").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Major),
        "ao DOL=\"SRC MS\" closed_loop: softIoc gives MAJOR/LINK"
    );
    assert_eq!(
        alarm(&db, "B1").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Major),
        "bo DOL=\"SRC MS\" closed_loop: softIoc gives MAJOR/LINK"
    );
}

/// SIML (`recGblGetSimm` → `dbTryGetLink`) and SIOL (`readValue` →
/// `dbGetLink`) both run the tail.
#[epics_macros_rs::epics_test]
async fn siml_and_siol_inherit_ms() {
    let db = db_with_major_source().await;
    // SIMM must stay out of simulation for the SIML case, so the source's
    // value is what SIMM reads: park a second source at 0 (menuSimm "NO") that
    // is nonetheless in MAJOR — C's `field(HIGH,"-1")` trick.
    db.add_record("SRC0", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("SRC0").unwrap();
        let mut inst = rec.write();
        inst.common.sevr = AlarmSeverity::Major;
        inst.common.stat = alarm_status::HIGH_ALARM;
    }

    db.add_record("R3", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv("R3.SIML", EpicsValue::String("SRC0 MS".into()))
        .await
        .unwrap();

    db.add_record("R4", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv("R4.SIML", EpicsValue::String("SRC0".into()))
        .await
        .unwrap();

    // SIOL is read only in simulation mode (SIMM=YES).
    db.add_record("R2", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv("R2.SIOL", EpicsValue::String("SRC MS".into()))
        .await
        .unwrap();
    db.put_pv("R2.SIMM", EpicsValue::Short(1)).await.unwrap();

    process(&db, "R3").await;
    process(&db, "R4").await;
    process(&db, "R2").await;

    assert_eq!(
        alarm(&db, "R3").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Major),
        "SIML=\"SRC0 MS\": softIoc gives MAJOR/LINK"
    );
    assert_eq!(
        alarm(&db, "R4").await.1,
        AlarmSeverity::NoAlarm,
        "SIML without MS inherits nothing (softIoc: NO_ALARM)"
    );
    assert_eq!(
        alarm(&db, "R2").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Major),
        "SIOL=\"SRC MS\" in simulation: softIoc gives MAJOR/LINK"
    );
    // The simulated read still landed.
    let v = db.get_pv("R2.VAL").unwrap().to_f64().unwrap();
    assert_eq!(v, 5.0, "the SIOL value reaches VAL through SVAL");
}

/// SDIS (`dbAccess.c:566`) and TSEL (`recGbl.c:315`) are `dbGetLink` reads too,
/// so they inherit as well — the rule is the read, not the field.
#[epics_macros_rs::epics_test]
async fn sdis_and_tsel_inherit_ms() {
    let db = db_with_major_source().await;

    db.add_record("D1", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv("D1.SDIS", EpicsValue::String("SRC MS".into()))
        .await
        .unwrap();
    // DISV=1 and the source delivers 5, so the record is NOT disabled and its
    // alarm is the inherited one.
    db.put_pv("D1.DISV", EpicsValue::Short(1)).await.unwrap();

    db.add_record("T2", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.put_pv("T2.TSEL", EpicsValue::String("SRC MS".into()))
        .await
        .unwrap();

    process(&db, "D1").await;
    process(&db, "T2").await;

    assert_eq!(
        alarm(&db, "D1").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Major),
        "SDIS=\"SRC MS\": softIoc gives MAJOR/LINK"
    );
    assert_eq!(
        alarm(&db, "T2").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Major),
        "TSEL=\"SRC MS\": softIoc gives MAJOR/LINK"
    );
}
