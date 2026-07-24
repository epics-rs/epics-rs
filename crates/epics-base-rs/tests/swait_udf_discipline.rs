//! swait's UDF is C's UDF: cleared only by a successful calc (or SIOL read),
//! never set, never an alarm (R11-C13).
//!
//! `swaitRecord.c` mentions `udf` on exactly two lines, and both CLEAR it:
//!
//! ```c
//!   :409  if (calcPerform(&pwait->a,&pwait->val,pwait->rpcl)) {
//!   :410      recGblSetSevr(pwait,CALC_ALARM,INVALID_ALARM);
//!   :411  } else pwait->udf = FALSE;
//!   …
//!   :417  status = dbGetLink(&(pwait->siol),DBR_DOUBLE,&(pwait->sval),0,0);
//!   :418  if (status==0) {
//!   :419      pwait->val=pwait->sval; pwait->udf=FALSE;
//!   :420  }
//! ```
//!
//! There is no `udf = TRUE` and no `checkAlarms` — swait names UDF_ALARM
//! nowhere, so an undefined swait raises no alarm from UDF. Two consequences
//! the port got wrong by deriving `udf = value_is_undefined()` every cycle in
//! the framework:
//!
//!   * a `0/0` VAL is NaN, but base's `calcPerform` has no isnan check (that is
//!     `sCalcPerform`), so it returns 0 → C CLEARS UDF and raises nothing; the
//!     port set UDF and reported UDF_ALARM/INVALID.
//!   * a swait whose calc has never once succeeded keeps `UDF=1` in C; the port
//!     cleared it on the first cycle merely because VAL (still its initial 0)
//!     was not NaN.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;
use std::collections::HashSet;

const CALC_ALARM: u16 = 12;

async fn process(db: &PvDatabase, name: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
}

async fn udf(db: &PvDatabase, name: &str) -> bool {
    db.get_record(name).unwrap().read().common.udf != 0
}

async fn alarm(db: &PvDatabase, name: &str) -> (AlarmSeverity, u16) {
    let g = db.get_record(name).unwrap();
    let g = g.read();
    (g.common.sevr, g.common.stat)
}

/// A NaN result is a calcPerform SUCCESS in base C — UDF is cleared and no
/// alarm is raised. The port used to report UDF_ALARM/INVALID here.
#[epics_macros_rs::epics_test]
async fn r11_c13_a_nan_result_clears_udf_and_raises_no_alarm() {
    let db = PvDatabase::new();
    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("0/0".into()))
        .unwrap();
    w.special("CALC", true).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    process(&db, "W").await;

    let val = db.get_record("W").unwrap().read().record.get_field("VAL");
    match val {
        Some(EpicsValue::Double(v)) => assert!(v.is_nan(), "0/0 leaves VAL NaN"),
        other => panic!("VAL: {other:?}"),
    }
    assert!(
        !udf(&db, "W").await,
        "swaitRecord.c:411 — calcPerform returned 0, so UDF is cleared, NaN or not"
    );
    assert_eq!(
        alarm(&db, "W").await,
        (AlarmSeverity::NoAlarm, 0),
        "swait raises UDF_ALARM nowhere in C"
    );
}

/// A calc that FAILS never clears UDF: C only clears it in the `else` arm.
/// The port cleared it anyway (VAL, still 0.0, is not NaN).
#[epics_macros_rs::epics_test]
async fn r11_c13_a_failing_calc_leaves_udf_set() {
    let db = PvDatabase::new();
    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("1+".into()))
        .unwrap(); // uncompilable → fails every cycle
    w.special("CALC", true).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    process(&db, "W").await;
    assert!(
        udf(&db, "W").await,
        "swaitRecord.c:409-411 — a failed calcPerform is the `if` arm; UDF is untouched"
    );
    assert_eq!(
        alarm(&db, "W").await,
        (AlarmSeverity::Invalid, CALC_ALARM),
        "the failure's only alarm is CALC_ALARM — never UDF_ALARM"
    );

    // And a later success clears it (C `:411`), from the same record.
    db.put_record_field_from_ca("W", "CALC", EpicsValue::String("7".into()))
        .await
        .unwrap();
    process(&db, "W").await;
    assert!(!udf(&db, "W").await, "a successful calcPerform clears UDF");
    assert_eq!(alarm(&db, "W").await, (AlarmSeverity::NoAlarm, 0));
}

/// The fetch gate: C runs no calcPerform, so UDF freezes at whatever it was
/// (here: still set — no calc has ever succeeded), and READ_ALARM is the only
/// alarm.
#[epics_macros_rs::epics_test]
async fn r11_c13_a_gated_cycle_does_not_touch_udf() {
    const READ_ALARM: u16 = 1;
    let db = PvDatabase::new();
    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A+1".into()))
        .unwrap();
    w.special("CALC", true).unwrap();
    w.put_field("INAN", EpicsValue::String("NOSUCHREC".into()))
        .unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    process(&db, "W").await;
    assert!(
        udf(&db, "W").await,
        "the gate skipped calcPerform, so C's `udf = FALSE` never ran"
    );
    assert_eq!(
        alarm(&db, "W").await,
        (AlarmSeverity::Invalid, READ_ALARM),
        "swaitRecord.c:412-414 — READ_ALARM, and no UDF_ALARM behind it"
    );
}
