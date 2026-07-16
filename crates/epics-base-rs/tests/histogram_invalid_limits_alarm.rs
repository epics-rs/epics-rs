//! CBUG-F12 — a histogram with `LLIM >= ULIM` and its invalid-limits alarm.
//!
//! C's `add_count` (histogramRecord.c:329-334) refuses to count when the limits
//! are inverted and raises SOFT_ALARM / INVALID_ALARM for it — but it writes
//! `prec->stat` and `prec->sevr` DIRECTLY, not `nsta`/`nsev` via `recGblSetSevr`.
//! That single mechanism yields two OBSERVABLE behaviours, depending on whether
//! a `recGblResetAlarms` runs after the write:
//!
//! * process path (`process()` → `monitor()`): the cycle's `recGblResetAlarms`
//!   copies `nsta/nsev`(0/0) over the direct write and erases it before any
//!   client can see it — STAT=NO_ALARM (CBUG-F12; compiled softIoc, this host:
//!   `LLIM=10 ULIM=5`, process → STAT=NO_ALARM SEVR=NO_ALARM).
//! * SGNL SPC_MOD `special()` path: `add_count` runs with no monitor after it,
//!   so the direct write STICKS — a `caget` reports STAT=SOFT SEVR=INVALID
//!   (compiled softIoc: fresh histogram, `caput .SGNL 1` → STAT=SOFT).
//!
//! The port reproduces C's exact mechanism: `check_alarms` writes `stat`/`sevr`
//! directly, so it is erased on the process path and persists on the SGNL
//! special path — matching C on both. (An earlier port shape raised it through
//! `recGblSetSevr`, which made `recGblResetAlarms` COMMIT it on the process path
//! — a stuck SOFT that contradicted the compiled-C proof and the oracle.)

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, name: &str) {
    db.process_record(name).await.unwrap();
}

/// (SEVR, STAT) as a client would read them.
async fn alarm(db: &PvDatabase, name: &str) -> (AlarmSeverity, u16) {
    let rec = db.get_record(name).await.unwrap();
    let inst = rec.read().await;
    (inst.common.sevr, inst.common.stat)
}

#[tokio::test]
async fn inverted_limits_alarm_is_erased_by_the_process_cycle() {
    let db = PvDatabase::new();
    // LLIM=10, ULIM=5 — the compiled-C proof case.
    db.add_record("H1", Box::new(HistogramRecord::new(16, 10.0, 5.0)))
        .await
        .unwrap();

    process(&db, "H1").await;

    assert_eq!(
        alarm(&db, "H1").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "add_count writes SOFT directly, monitor()'s recGblResetAlarms erases it \
         in the same cycle — NO_ALARM (CBUG-F12)"
    );
}

/// The process path always reads NO_ALARM: the direct write is erased every
/// cycle, inverted or not. Fixing the limits changes nothing observable here.
#[tokio::test]
async fn process_path_reads_no_alarm_inverted_or_valid() {
    let db = PvDatabase::new();
    db.add_record("H2", Box::new(HistogramRecord::new(16, 10.0, 5.0)))
        .await
        .unwrap();

    process(&db, "H2").await;
    process(&db, "H2").await;
    assert_eq!(
        alarm(&db, "H2").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "inverted, but the direct SOFT write is erased by recGblResetAlarms"
    );

    db.put_pv("H2.ULIM", EpicsValue::Double(20.0))
        .await
        .unwrap();
    process(&db, "H2").await;
    assert_eq!(
        alarm(&db, "H2").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "LLIM=10 < ULIM=20: valid limits raise nothing either"
    );
}

/// Equal limits are inverted limits (C's test is `>=`); on the process path they
/// too read NO_ALARM.
#[tokio::test]
async fn equal_limits_erased_on_process_too() {
    let db = PvDatabase::new();
    db.add_record("H3", Box::new(HistogramRecord::new(16, 5.0, 5.0)))
        .await
        .unwrap();

    process(&db, "H3").await;

    assert_eq!(
        alarm(&db, "H3").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "LLIM == ULIM is the `>=` boundary — still erased on the process cycle"
    );
}

/// The SGNL SPC_MOD special path has no monitor after `add_count`, so the direct
/// SOFT/INVALID write persists — a client reads STAT=SOFT (compiled softIoc:
/// `caput .SGNL 1` on inverted limits → STAT=SOFT).
#[tokio::test]
async fn sgnl_special_path_sticks_the_soft_alarm() {
    let db = PvDatabase::new();
    db.add_record("H5", Box::new(HistogramRecord::new(16, 10.0, 5.0)))
        .await
        .unwrap();

    // A SGNL caput is C's SPC_MOD special() -> add_count; no process follows.
    db.put_pv("H5.SGNL", EpicsValue::Double(1.0)).await.unwrap();

    assert_eq!(
        alarm(&db, "H5").await,
        (AlarmSeverity::Invalid, alarm_status::SOFT_ALARM),
        "no monitor after add_count on the special path — the direct write sticks"
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
