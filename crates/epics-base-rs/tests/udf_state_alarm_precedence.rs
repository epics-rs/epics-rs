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

async fn alarm(db: &PvDatabase) -> (AlarmSeverity, u16) {
    let inst = db.get_record("REC").await.unwrap();
    let g = inst.read().await;
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
