//! CBUG-F12 REFUSED — a histogram with `LLIM > ULIM` and its inverted-limits
//! alarm.
//!
//! C's `add_count` (histogramRecord.c:329-334) refuses to count when the limits
//! are inverted and *tries* to raise SOFT_ALARM / INVALID_ALARM for it — but it
//! writes `prec->stat`/`prec->sevr` DIRECTLY, not `nsta`/`nsev` via
//! `recGblSetSevr`. That broken mechanism makes the alarm's visibility depend on
//! which trigger ran `add_count`:
//!
//! * process path (`process()` → `monitor()`): the cycle's `recGblResetAlarms`
//!   copies `nsta/nsev`(0/0) over the direct write and erases it — C shows
//!   NO_ALARM (dead code: C's intent to alarm, C's behaviour NO_ALARM).
//! * SGNL SPC_MOD `special()` path: no monitor follows, so the direct write
//!   STICKS and a `caget` reports STAT=SOFT SEVR=INVALID.
//!
//! Per `doc/strategy-2026-07-13.md` §2 — *clean is the goal* — the port REFUSES
//! that path-dependent dead-code/sticky behaviour. An inverted-limits histogram
//! is genuinely misconfigured (it can bin nothing), so the port raises the
//! alarm through the single `nsta`/`nsev` owner and reports SOFT/INVALID
//! CONSISTENTLY on BOTH paths: committed by `recGblResetAlarms` on the process
//! path, and committed by the SGNL special path's own post-`check_alarms`
//! `recGblResetAlarms`. (This is a Tier-2 deviation from a compiled C IOC on the
//! process path, where C erases the alarm; recorded as a CBUG-F12 allowlist
//! row.)
//!
//! The refusal covers an INVERSION (`LLIM > ULIM`) and stops there. C's counting
//! test is `>=`, but `LLIM == ULIM` is the state every histogram is in before
//! anyone configures it — neither field carries an `initial(...)` in
//! `histogramRecord.dbd`, so a bare `record(histogram, "…") {}` loads with
//! `LLIM == ULIM == 0` and `CSTA == 1`. Alarming there made every unconfigured
//! histogram publish SOFT/INVALID on its first process against C's NO_ALARM,
//! which the differential oracle measured as 14 defects. Refusing C's broken
//! mechanism is not a licence to alarm on C's default state.

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
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (inst.common.sevr, inst.common.stat)
}

/// The process path raises the misconfiguration alarm through `nsta`/`nsev`, so
/// `recGblResetAlarms` COMMITS it (rather than erasing a direct write) —
/// SOFT/INVALID. C erases it here (NO_ALARM); the port refuses that dead code.
#[epics_macros_rs::epics_test]
async fn inverted_limits_alarm_is_raised_on_the_process_cycle() {
    let db = PvDatabase::new();
    // LLIM=10, ULIM=5 — the compiled-C proof case.
    db.add_record("H1", Box::new(HistogramRecord::new(16, 10.0, 5.0)))
        .await
        .unwrap();

    process(&db, "H1").await;

    assert_eq!(
        alarm(&db, "H1").await,
        (AlarmSeverity::Invalid, alarm_status::SOFT_ALARM),
        "inverted limits raise SOFT/INVALID through nsta/nsev; recGblResetAlarms \
         commits it — CBUG-F12 refused (C erases it to NO_ALARM)"
    );
}

/// Inverted limits alarm SOFT/INVALID every process cycle; fixing the limits to
/// valid clears the alarm on the next cycle.
#[epics_macros_rs::epics_test]
async fn process_path_alarms_while_inverted_clears_when_valid() {
    let db = PvDatabase::new();
    db.add_record("H2", Box::new(HistogramRecord::new(16, 10.0, 5.0)))
        .await
        .unwrap();

    process(&db, "H2").await;
    process(&db, "H2").await;
    assert_eq!(
        alarm(&db, "H2").await,
        (AlarmSeverity::Invalid, alarm_status::SOFT_ALARM),
        "inverted: SOFT/INVALID raised and committed every cycle (CBUG-F12 refused)"
    );

    db.put_pv("H2.ULIM", EpicsValue::Double(20.0))
        .await
        .unwrap();
    process(&db, "H2").await;
    assert_eq!(
        alarm(&db, "H2").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "LLIM=10 < ULIM=20: valid limits raise nothing, and the prior alarm clears"
    );
}

/// Equal limits are an EMPTY range, not an inverted one, and they are what an
/// unconfigured record has. They bin nothing (C's counting test is `>=`) but
/// they raise no alarm, because no operator has expressed anything to be wrong
/// about.
#[epics_macros_rs::epics_test]
async fn equal_limits_do_not_alarm() {
    let db = PvDatabase::new();
    db.add_record("H3", Box::new(HistogramRecord::new(16, 5.0, 5.0)))
        .await
        .unwrap();

    process(&db, "H3").await;

    assert_eq!(
        alarm(&db, "H3").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "LLIM == ULIM is an empty range, not an inversion — nothing is raised"
    );
}

/// The oracle's 14 defects, as one in-process case: a record loaded with no
/// `field(LLIM,…)` / `field(ULIM,…)` at all. `histogramRecord.dbd` gives neither
/// an `initial(...)`, so both are 0 and `CSTA` is 1 — and C publishes NO_ALARM
/// after a `caput .PROC 1` on exactly this record.
#[epics_macros_rs::epics_test]
async fn a_freshly_loaded_histogram_does_not_alarm() {
    let db = PvDatabase::new();
    db.add_record("H6", Box::new(HistogramRecord::new(16, 0.0, 0.0)))
        .await
        .unwrap();

    process(&db, "H6").await;

    assert_eq!(
        alarm(&db, "H6").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "an unconfigured histogram is not a misconfigured one — C shows NO_ALARM here"
    );
}

/// The SGNL SPC_MOD special path runs `check_alarms` then commits it via
/// `recGblResetAlarms`, so the SOFT/INVALID is observable — same as the process
/// path, and same value C's sticky direct write happens to leave here.
#[epics_macros_rs::epics_test]
async fn sgnl_special_path_raises_the_soft_alarm() {
    let db = PvDatabase::new();
    db.add_record("H5", Box::new(HistogramRecord::new(16, 10.0, 5.0)))
        .await
        .unwrap();

    // A SGNL caput is C's SPC_MOD special() -> add_count; no process follows,
    // but the port commits the alarm on the special path.
    db.put_pv("H5.SGNL", EpicsValue::Double(1.0)).await.unwrap();

    assert_eq!(
        alarm(&db, "H5").await,
        (AlarmSeverity::Invalid, alarm_status::SOFT_ALARM),
        "special path commits the nsta/nsev alarm — SOFT/INVALID observable (CBUG-F12)"
    );
}

/// The negative control: valid limits raise nothing on either path. (A histogram
/// never clears UDF and has no UDF alarm, so a well-formed one publishes
/// NO_ALARM even though UDF stays 1 — `udf_clear_is_per_record_type` pins that.)
#[epics_macros_rs::epics_test]
async fn valid_limits_raise_no_alarm() {
    let db = PvDatabase::new();
    db.add_record("H4", Box::new(HistogramRecord::new(16, 0.0, 10.0)))
        .await
        .unwrap();

    process(&db, "H4").await;
    assert_eq!(
        alarm(&db, "H4").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "LLIM < ULIM: nothing to alarm about (process path)"
    );

    // And the SGNL special path with valid limits also stays quiet.
    db.put_pv("H4.SGNL", EpicsValue::Double(1.0)).await.unwrap();
    assert_eq!(
        alarm(&db, "H4").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "valid limits: the SGNL special path's commit leaves NO_ALARM"
    );
}
