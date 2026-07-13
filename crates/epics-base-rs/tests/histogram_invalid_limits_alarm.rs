//! CBUG-F12 — a histogram with `LLIM >= ULIM` raises the invalid-limits alarm.
//!
//! DEVIATION from C, deliberate. C's `add_count` (histogramRecord.c:328-334)
//! refuses to count when the limits are inverted and raises SOFT_ALARM /
//! INVALID_ALARM for it — but it writes `prec->stat` and `prec->sevr` DIRECTLY
//! instead of `nsta`/`nsev` via `recGblSetSevr`. The same process cycle then
//! calls `monitor()`, whose `recGblResetAlarms()` copies `nsta/nsev → stat/sevr`
//! and erases the write before any client can see it. The alarm is dead code:
//! C's intent is to alarm, C's behaviour is NO_ALARM (compiled softIoc, this
//! host: `LLIM=10 ULIM=5`, process → STAT=NO_ALARM SEVR=NO_ALARM).
//!
//! The port raises it for real, through `recGblSetSevr` — so `recGblResetAlarms`
//! promotes it instead of erasing it.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, name: &str) {
    db.process_record(name).await.unwrap();
}

/// (SEVR, STAT) after processing.
async fn alarm(db: &PvDatabase, name: &str) -> (AlarmSeverity, u16) {
    let rec = db.get_record(name).await.unwrap();
    let inst = rec.read().await;
    (inst.common.sevr, inst.common.stat)
}

#[tokio::test]
async fn inverted_limits_alarm_survives_the_process_cycle() {
    let db = PvDatabase::new();
    // LLIM=10, ULIM=5 — the compiled-C proof case.
    db.add_record("H1", Box::new(HistogramRecord::new(16, 10.0, 5.0)))
        .await
        .unwrap();

    process(&db, "H1").await;

    assert_eq!(
        alarm(&db, "H1").await,
        (AlarmSeverity::Invalid, alarm_status::SOFT_ALARM),
        "inverted limits must alarm (C erases this in the same cycle — CBUG-F12)"
    );
}

/// The alarm is a level, not an edge: it holds for as long as the limits are
/// inverted, and clears when they are not.
#[tokio::test]
async fn the_alarm_holds_while_inverted_and_clears_when_fixed() {
    let db = PvDatabase::new();
    db.add_record("H2", Box::new(HistogramRecord::new(16, 10.0, 5.0)))
        .await
        .unwrap();

    process(&db, "H2").await;
    process(&db, "H2").await;
    assert_eq!(
        alarm(&db, "H2").await,
        (AlarmSeverity::Invalid, alarm_status::SOFT_ALARM),
        "still inverted on the second cycle, so still alarming"
    );

    db.put_pv("H2.ULIM", EpicsValue::Double(20.0))
        .await
        .unwrap();
    process(&db, "H2").await;
    assert_eq!(
        alarm(&db, "H2").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "LLIM=10 < ULIM=20: the limits are valid again"
    );
}

/// Equal limits are inverted limits: C's test is `>=`, and a zero-width
/// histogram can bin nothing.
#[tokio::test]
async fn equal_limits_alarm_too() {
    let db = PvDatabase::new();
    db.add_record("H3", Box::new(HistogramRecord::new(16, 5.0, 5.0)))
        .await
        .unwrap();

    process(&db, "H3").await;

    assert_eq!(
        alarm(&db, "H3").await,
        (AlarmSeverity::Invalid, alarm_status::SOFT_ALARM),
        "LLIM == ULIM is the `>=` boundary"
    );
}

/// The negative control: valid limits raise nothing. (A histogram never clears
/// UDF and has no UDF alarm, so a well-formed one publishes NO_ALARM even though
/// UDF stays 1 — `udf_clear_is_per_record_type` pins that half.)
#[tokio::test]
async fn valid_limits_raise_no_alarm() {
    let db = PvDatabase::new();
    db.add_record("H4", Box::new(HistogramRecord::new(16, 0.0, 10.0)))
        .await
        .unwrap();

    process(&db, "H4").await;

    assert_eq!(
        alarm(&db, "H4").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "LLIM < ULIM: nothing to alarm about"
    );
}
