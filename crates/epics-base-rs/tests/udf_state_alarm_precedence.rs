//! Cause 2 (Family C): on a fresh output record, the UDF alarm must win STAT
//! over an equal-severity STATE alarm.
//!
//! A bare `record(bo,"X"){}` has VAL=0 and UDF=1. `caput X.ZSV 3` sets the
//! zero-state severity to INVALID; ZSV is `field(ZSV,DBF_MENU){ pp(TRUE) }`
//! (boRecord.dbd:93-99), so the put processes the record. C `boRecord.c`
//! `checkAlarms` raises the UDF alarm FIRST (`:371-373`,
//! `recGblSetSevr(UDF_ALARM, udfs=INVALID)`) then the STATE alarm
//! (`:376-380`, `recGblSetSevr(STATE_ALARM, zsv=INVALID)`). `recGblSetSevr`
//! overrides only on strictly greater severity (recGbl.c:242,
//! `if (nsev < new_sevr)`), so the equal-severity STATE alarm does NOT displace
//! the UDF already set — STAT stays UDF, SEVR INVALID.
//!
//! mbbo is the same shape: C raises UDF in `process()` (`mbboRecord.c:210-212`)
//! before `checkAlarms` runs the ZRSV state alarm.
//!
//! The port raised the STATE alarm first (in the record's `check_alarms` hook)
//! and the UDF alarm second (in the framework `evaluate_alarms`, which runs
//! after), so the equal-severity UDF could not displace STATE and STAT came out
//! STATE. Verified against the C source, not the running oracle.

// RTEMS-EXEC-MODEL-ALLOW(7): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_base_rs::types::EpicsValue;

const UDF_ALARM: u16 = 17;
const STATE_ALARM: u16 = 7;

async fn db_with(record: Box<dyn Record>) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("REC", record).await.unwrap();
    db
}

async fn caput(db: &PvDatabase, field: &str, text: &str) {
    db.put_record_field_from_ca("REC", field, EpicsValue::String(text.into()))
        .await
        .unwrap();
}

/// A **numeric** put of a raw severity ordinal into a `DBF_MENU` selector — the
/// wire path a `caput .ZSV 4` / `caput .ZSV -1` takes. A DBR_SHORT/LONG/ENUM put
/// skips `putStringMenu`'s `< nChoice` label bound (which refuses a string "4")
/// and stores the raw `epicsEnum16` via `putEnum` (`*pfield = (epicsEnum16)val`):
/// `4 -> 4`, `-1 -> 65535`. The port stores it as the field's native `i16`
/// (`-1i16` reads back as raw `u16` 65535 in the severity compare).
async fn caput_raw(db: &PvDatabase, field: &str, ordinal: i16) {
    db.put_record_field_from_ca("REC", field, EpicsValue::Short(ordinal))
        .await
        .unwrap();
}

async fn alarm(db: &PvDatabase) -> (AlarmSeverity, u16) {
    let inst = db.get_record("REC").unwrap();
    let g = inst.read();
    (g.common.sevr, g.common.stat)
}

/// Fresh bo (VAL=0, UDF=1), `caput ZSV 3` (INVALID). STAT=UDF, SEVR=INVALID —
/// the equal-severity STATE alarm does not overwrite the leading UDF.
#[tokio::test]
async fn bo_udf_beats_equal_severity_state() {
    let db = db_with(Box::new(BoRecord::new(0))).await;
    caput(&db, "ZSV", "3").await;
    assert_eq!(
        alarm(&db).await,
        (AlarmSeverity::Invalid, UDF_ALARM),
        "UDF is raised before STATE and equal severity does not override"
    );
}

/// Fresh mbbo (VAL=0, UDF=1), `caput ZRSV 3` (INVALID). STAT=UDF, SEVR=INVALID.
#[tokio::test]
async fn mbbo_udf_beats_equal_severity_state() {
    let db = db_with(Box::new(MbboRecord::new(0))).await;
    caput(&db, "ZRSV", "3").await;
    assert_eq!(
        alarm(&db).await,
        (AlarmSeverity::Invalid, UDF_ALARM),
        "UDF is raised before ZRSV STATE and equal severity does not override"
    );
}

/// Out-of-range STATE severity ordinal (`ZSV=4`, over-max) DOES override the
/// prior UDF: C `recGblSetSevr(STATE_ALARM, prec->zsv)` compares the RAW
/// `epicsEnum16` (recGbl.c:242), and `3 < 4` is true, so STAT=STATE. The
/// displayed SEVR stays INVALID (`recGblResetAlarms` clamps). This is the
/// regression the raw-ordinal compare fixes — clamping `zsv=4` to `Invalid(3)`
/// before the compare would tie UDF and wrongly leave STAT=UDF.
#[tokio::test]
async fn bo_over_max_state_severity_overrides_udf() {
    let db = db_with(Box::new(BoRecord::new(0))).await;
    caput_raw(&db, "ZSV", 4).await;
    assert_eq!(
        alarm(&db).await,
        (AlarmSeverity::Invalid, STATE_ALARM),
        "raw zsv=4 > udfs=3, so STATE overrides UDF; SEVR clamped to INVALID"
    );
}

/// Negative STATE severity ordinal (`ZSV=-1` → `65535`) also overrides: the raw
/// `DBF_MENU` field holds `65535`, numerically greater than UDFS's `3`.
#[tokio::test]
async fn bo_negative_state_severity_overrides_udf() {
    let db = db_with(Box::new(BoRecord::new(0))).await;
    caput_raw(&db, "ZSV", -1).await;
    assert_eq!(
        alarm(&db).await,
        (AlarmSeverity::Invalid, STATE_ALARM),
        "raw zsv=65535 > udfs=3, so STATE overrides UDF; SEVR clamped to INVALID"
    );
}

#[tokio::test]
async fn mbbo_over_max_state_severity_overrides_udf() {
    let db = db_with(Box::new(MbboRecord::new(0))).await;
    caput_raw(&db, "ZRSV", 4).await;
    assert_eq!(
        alarm(&db).await,
        (AlarmSeverity::Invalid, STATE_ALARM),
        "raw zrsv=4 > udfs=3, so STATE overrides UDF; SEVR clamped to INVALID"
    );
}

#[tokio::test]
async fn mbbo_negative_state_severity_overrides_udf() {
    let db = db_with(Box::new(MbboRecord::new(0))).await;
    caput_raw(&db, "ZRSV", -1).await;
    assert_eq!(
        alarm(&db).await,
        (AlarmSeverity::Invalid, STATE_ALARM),
        "raw zrsv=65535 > udfs=3, so STATE overrides UDF; SEVR clamped to INVALID"
    );
}

/// The precedence is severity-driven, not a blanket "UDF always wins": once the
/// record is defined (UDF=0), the STATE alarm from ZSV stands on its own. A VAL
/// put clears UDF (bo `clears_udf()==false`, but a VAL put defines it), and a
/// following ZSV put then yields STATE, proving UDF is not suppressing STATE
/// unconditionally.
#[tokio::test]
async fn bo_state_alarm_stands_when_defined() {
    let db = db_with(Box::new(BoRecord::new(0))).await;
    // Define VAL=0 (clears UDF).
    caput(&db, "VAL", "0").await;
    // ZSV=3 with a defined record -> STATE/INVALID, no UDF.
    caput(&db, "ZSV", "3").await;
    assert_eq!(
        alarm(&db).await,
        (AlarmSeverity::Invalid, STATE_ALARM),
        "with UDF cleared, the ZSV state alarm owns STAT"
    );
}
