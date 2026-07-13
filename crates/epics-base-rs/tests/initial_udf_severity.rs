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
#[tokio::test]
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
        let rec = db.get_record(name).await.unwrap();
        let inst = rec.read().await;
        assert!(inst.common.udf, "{name}: never processed, so UDF");
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
#[tokio::test]
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

    let rec = db.get_record("CON").await.unwrap();
    let inst = rec.read().await;
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

/// UDFS is the severity the rule uses — a record that declares
/// `field(UDFS,"MINOR")` starts MINOR, not INVALID.
#[tokio::test]
async fn the_initial_severity_comes_from_udfs() {
    let db = build(r#"record(ai, "A2") { field(UDFS, "MINOR") }"#).await;
    let rec = db.get_record("A2").await.unwrap();
    let inst = rec.read().await;
    assert_eq!(inst.common.sevr, AlarmSeverity::Minor);
    assert_eq!(inst.common.stat, alarm_status::UDF_ALARM);
}

/// A record whose value is defined at init (a constant INP loaded by the
/// init-seed owner) clears UDF on its first process and the initial severity
/// goes away — the alarm is not sticky.
#[tokio::test]
async fn the_initial_severity_clears_on_the_first_successful_process() {
    let db = build(r#"record(calc, "C2") { field(INPA, "5") field(CALC, "A+1") }"#).await;

    let mut v = HashSet::new();
    db.process_record_with_links("C2", &mut v, 0).await.unwrap();

    let rec = db.get_record("C2").await.unwrap();
    let inst = rec.read().await;
    assert!(!inst.common.udf);
    assert_eq!(inst.common.stat, alarm_status::NO_ALARM);
    assert_eq!(inst.common.sevr, AlarmSeverity::NoAlarm);
}
