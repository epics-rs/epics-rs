// RTEMS-EXEC-MODEL-ALLOW(1): IocShell::new takes a tokio Handle, so one test needs an ambient tokio runtime; runs and passes in the feature-ON suite.
//! R16-82: the initial UDF severity — a record that has never processed is
//! INVALID, not NO_ALARM.
//!
//! C, `iocInit.c::doInitRecord0` (:508-536), run on EVERY record before
//! `init_record` pass 0:
//!
//! ```c
//! /* Initial UDF severity */
//! if (precord->udf && precord->stat == UDF_ALARM)
//!     precord->sevr = precord->udfs;
//! ```
//!
//! with `dbCommon.dbd.pod`: UDF `initial("1")`, STAT `initial("UDF")`, UDFS
//! `initial("INVALID")`.
//!
//! softIoc (EPICS 7.0.10, linux-x86_64):
//!
//! ```text
//! record(ai,"SRC"){}
//! record(calc,"CON"){field(INPA,"SRC MS") field(CALC,"A")}
//!
//! iocInit        -> SRC.STAT = UDF, SRC.SEVR = INVALID, SRC.UDF = 1
//! dbpf CON.PROC 1-> CON.STAT = LINK, CON.SEVR = INVALID
//! ```
//!
//! The port left a fresh record at STAT=0/SEVR=NO_ALARM with UDF=1, so an MS
//! consumer inherited NOTHING from a not-yet-processed source — the IOC-startup
//! ordering case MS exists for.

use std::collections::HashMap;
use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;

async fn build(db_text: &str) -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// Every record type: born UDF, so `iocInit` publishes STAT=UDF SEVR=INVALID.
#[epics_macros_rs::epics_test]
async fn a_never_processed_record_is_udf_invalid_after_init() {
    let db = build(
        r#"
        record(ai, "A1") {}
        record(calc, "C1") { field(CALC, "1") }
        record(bi, "B1") {}
        record(waveform, "W1") { field(FTVL, "DOUBLE") field(NELM, "4") }
        "#,
    )
    .await;

    for name in ["A1", "C1", "B1", "W1"] {
        let rec = db.get_record(name).unwrap();
        let inst = rec.read();
        assert!(inst.common.udf != 0, "{name}: never processed, so UDF");
        assert_eq!(
            inst.common.stat,
            alarm_status::UDF_ALARM,
            "{name}: STAT is born UDF (dbCommon.dbd initial(\"UDF\"))"
        );
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::Invalid,
            "{name}: iocInit raises SEVR to UDFS while udf && stat == UDF_ALARM"
        );
    }
}

/// The case the finding is about: an `MS` consumer of a not-yet-processed
/// source inherits INVALID and reports LINK — the IOC-startup ordering the MS
/// modifier exists for. softIoc: CON.STAT=LINK, CON.SEVR=INVALID.
#[epics_macros_rs::epics_test]
async fn ms_inherits_the_initial_udf_severity_from_an_unprocessed_source() {
    let db = build(
        r#"
        record(ai, "SRC") {}
        record(calc, "CON") { field(INPA, "SRC MS") field(CALC, "A") }
        "#,
    )
    .await;

    let mut v = HashSet::new();
    db.process_record_with_links("CON", &mut v, 0)
        .await
        .unwrap();

    let rec = db.get_record("CON").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "MS carries the source's INVALID across the link"
    );
    assert_eq!(
        inst.common.stat,
        alarm_status::LINK_ALARM,
        "C `recGblInheritSevrMsg` sets the consumer's status to LINK_ALARM"
    );
}

/// R19-23, the other half of the rule: a `.db` `field(VAL,…)` DEFINES the
/// record at load, so the seed above must NOT fire for it.
///
/// C `dbStaticLib.c:2653-2661` — `dbPutString` ends every successful put to a
/// field named `VAL` with `dbPutString(&udf_entry, "0")`, and that runs inside
/// `dbLoadRecords`, i.e. BEFORE `iocInit`'s `if (udf && stat == UDF_ALARM)`.
/// Measured, softIoc 7.0 (linux-x86_64), `record(ai,"QT1"){field(VAL,"3.14")}`
/// + `record(ai,"QT2"){}`, right after `iocInit`:
///
/// ```text
/// dbgf QT1.UDF -> 0   QT1.STAT -> UDF   QT1.SEVR -> NO_ALARM
/// dbgf QT2.UDF -> 1   QT2.STAT -> UDF   QT2.SEVR -> INVALID
/// ```
///
/// STAT stays UDF in both: `iocInit` lowers nothing, it only raises SEVR.
#[epics_macros_rs::epics_test]
async fn a_db_val_defines_the_record_so_the_seed_does_not_fire() {
    let db = build(
        r#"
        record(ai, "QT1") { field(VAL, "3.14") }
        record(bi, "QB1") { field(VAL, "1") }
        record(stringin, "QS1") { field(VAL, "hi") }
        record(calc, "QC1") { field(CALC, "1") field(VAL, "7") }
        record(ai, "QT2") {}
        "#,
    )
    .await;

    for name in ["QT1", "QB1", "QS1", "QC1"] {
        let rec = db.get_record(name).unwrap();
        let inst = rec.read();
        assert!(
            inst.common.udf == 0,
            "{name}: field(VAL,…) clears UDF at load (C dbPutString)"
        );
        assert_eq!(
            inst.common.stat,
            alarm_status::UDF_ALARM,
            "{name}: STAT is still born UDF — iocInit lowers nothing"
        );
        assert_eq!(
            inst.common.sevr,
            AlarmSeverity::NoAlarm,
            "{name}: the seed's `udf &&` precondition is false, so NO_ALARM"
        );
    }

    // The control: the same load, no VAL, still INVALID.
    let rec = db.get_record("QT2").unwrap();
    let inst = rec.read();
    assert!(inst.common.udf != 0);
    assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
}

/// The same rule on the runtime `dbLoadRecords` path — the second `.db` loader.
/// The seed is evaluated by the creation sink, which both loaders now hand
/// their complete field set to, so neither can init a half-loaded record.
///
/// `#[tokio::test]`, not `#[epics_test]`: `IocShell::new` takes a
/// `tokio::runtime::Handle`, so this body needs an ambient tokio runtime on
/// both backends (the census marker accounts for it).
#[tokio::test]
async fn the_runtime_db_loader_applies_the_val_udf_rule_too() {
    use epics_base_rs::server::iocsh::IocShell;

    let dir = std::env::temp_dir().join(format!("r19_23_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_file = dir.join("r19_23.db");
    std::fs::write(
        &db_file,
        "record(ai, \"RT1\") { field(VAL, \"2.5\") }\nrecord(ai, \"RT2\") {}\n",
    )
    .unwrap();

    let db = std::sync::Arc::new(PvDatabase::new());
    {
        // iocsh commands block on the runtime, so they run off the async thread.
        let (db, handle) = (db.clone(), tokio::runtime::Handle::current());
        let line = format!("dbLoadRecords(\"{}\")", db_file.display());
        std::thread::spawn(move || {
            IocShell::new(db, handle).execute_line(&line).unwrap();
        })
        .join()
        .unwrap();
    }
    db.ioc_init().await;

    let rec = db.get_record("RT1").unwrap();
    {
        let inst = rec.read();
        assert!(inst.common.udf == 0, "RT1: field(VAL,…) clears UDF at load");
        assert_eq!(inst.common.sevr, AlarmSeverity::NoAlarm);
    }

    let rec = db.get_record("RT2").unwrap();
    let inst = rec.read();
    assert!(inst.common.udf != 0);
    assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);

    std::fs::remove_dir_all(&dir).ok();
}

/// UDFS is the severity the rule uses — a record that declares
/// `field(UDFS,"MINOR")` starts MINOR, not INVALID.
#[epics_macros_rs::epics_test]
async fn the_initial_severity_comes_from_udfs() {
    let db = build(r#"record(ai, "A2") { field(UDFS, "MINOR") }"#).await;
    let rec = db.get_record("A2").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.sevr, AlarmSeverity::Minor);
    assert_eq!(inst.common.stat, alarm_status::UDF_ALARM);
}

/// A record whose value is defined at init (a constant INP loaded by the
/// init-seed owner) clears UDF on its first process and the initial severity
/// goes away — the alarm is not sticky.
#[epics_macros_rs::epics_test]
async fn the_initial_severity_clears_on_the_first_successful_process() {
    let db = build(r#"record(calc, "C2") { field(INPA, "5") field(CALC, "A+1") }"#).await;

    let mut v = HashSet::new();
    db.process_record_with_links("C2", &mut v, 0).await.unwrap();

    let rec = db.get_record("C2").unwrap();
    let inst = rec.read();
    assert!(inst.common.udf == 0);
    assert_eq!(inst.common.stat, alarm_status::NO_ALARM);
    assert_eq!(inst.common.sevr, AlarmSeverity::NoAlarm);
}
