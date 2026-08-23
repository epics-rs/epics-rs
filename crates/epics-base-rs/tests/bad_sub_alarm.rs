//! B1 — a non-empty but unregistered SNAM must raise BAD_SUB/INVALID on
//! EVERY process cycle, and aSub must publish `S_db_BadSub` as VAL.
//!
//! C `aSubRecord.c::do_sub` (454-473):
//!
//! ```c
//! if (prec->snam[0] == 0)
//!     return 0;
//! if (pfunc == NULL) {
//!     recGblSetSevr(prec, BAD_SUB_ALARM, INVALID_ALARM);
//!     return S_db_BadSub;
//! }
//! ```
//!
//! and `process` (aSubRecord.c:222-225) publishes that status:
//! `if (!status) { status = do_sub(prec); prec->val = status; }`.
//! `S_db_BadSub` is `(M_dbAccess | 35)` = `(511 << 16) | 35` = 33488931
//! (`dbAccessDefs.h:189`, `errMdef.h:39`).
//!
//! `subRecord.c::do_sub` (420-437) is the same raise WITHOUT the empty-SNAM
//! early return and returns 0 rather than `S_db_BadSub` — `sub` reaches it
//! with a null `sadr` only when it was not PACT-parked, i.e. exactly the
//! non-empty unregistered SNAM case.
//!
//! `iocInit` discards `init_record`'s status (`iocInit.c:569-570`), so the
//! record loads and scans: the alarm is re-raised every cycle, not once.
//!
//! The port had NO `BAD_SUB_ALARM` raise site anywhere — an aSub or sub whose
//! SNAM names a function that was never registered read back
//! `NO_ALARM NO_ALARM`, and aSub's VAL kept its previous value.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::sub_record::SubRecord;
use epics_base_rs::types::EpicsValue;

/// C `S_db_BadSub`, the value aSub's `process` writes into VAL.
const S_DB_BAD_SUB: i32 = (511 << 16) | 35;

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

/// `record(aSub,"X"){field(SNAM,"neverRegistered")}` — no subroutine is bound
/// because the registry has no such name, so every cycle raises
/// BAD_SUB/INVALID and publishes `S_db_BadSub` as VAL.
#[epics_macros_rs::epics_test]
async fn asub_unregistered_snam_raises_bad_sub_every_cycle() {
    let db = PvDatabase::new();
    let mut seed = ASubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("neverRegistered".into()))
        .unwrap();
    // A value VAL must be overwritten by the status, C `prec->val = status`.
    seed.put_field("VAL", EpicsValue::Long(7)).unwrap();
    db.add_record("X", Box::new(seed)).await.unwrap();

    process(&db, "X").await;
    assert_eq!(
        alarm(&db, "X").await,
        (alarm_status::BAD_SUB_ALARM, AlarmSeverity::Invalid),
        "C do_sub: recGblSetSevr(prec, BAD_SUB_ALARM, INVALID_ALARM)"
    );
    let rec = db.get_record("X").unwrap();
    assert_eq!(
        rec.read().record.get_field("VAL"),
        Some(EpicsValue::Long(S_DB_BAD_SUB)),
        "C process: prec->val = do_sub() = S_db_BadSub"
    );

    // Not a one-shot init diagnostic — iocInit discards init_record's status,
    // so the record keeps scanning and re-raises on every cycle.
    process(&db, "X").await;
    assert_eq!(
        alarm(&db, "X").await,
        (alarm_status::BAD_SUB_ALARM, AlarmSeverity::Invalid)
    );
}

/// An EMPTY SNAM is not a bad-sub for aSub: C returns 0 before the null check,
/// so the record stays healthy and VAL is forced to 0.
#[epics_macros_rs::epics_test]
async fn asub_empty_snam_is_not_a_bad_sub() {
    let db = PvDatabase::new();
    db.add_record("E", Box::new(ASubRecord::default()))
        .await
        .unwrap();

    process(&db, "E").await;
    assert_eq!(
        alarm(&db, "E").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "aSubRecord.c:459-460: an empty SNAM returns 0 before the pfunc check"
    );
    let rec = db.get_record("E").unwrap();
    assert_eq!(
        rec.read().record.get_field("VAL"),
        Some(EpicsValue::Long(0))
    );
}

/// `record(sub,"Y"){field(SNAM,"neverRegistered")}` — C raises the same
/// BAD_SUB/INVALID, but `sub`'s `do_sub` returns 0 and its VAL is the value
/// the subroutine computes, not a status, so VAL must not move.
#[epics_macros_rs::epics_test]
async fn sub_unregistered_snam_raises_bad_sub_and_leaves_val_alone() {
    let db = PvDatabase::new();
    let mut seed = SubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("neverRegistered".into()))
        .unwrap();
    seed.put_field("VAL", EpicsValue::Double(4.5)).unwrap();
    db.add_record("Y", Box::new(seed)).await.unwrap();

    process(&db, "Y").await;
    assert_eq!(
        alarm(&db, "Y").await,
        (alarm_status::BAD_SUB_ALARM, AlarmSeverity::Invalid),
        "subRecord.c:424-427: recGblSetSevr(prec, BAD_SUB_ALARM, INVALID_ALARM)"
    );
    let rec = db.get_record("Y").unwrap();
    assert_eq!(
        rec.read().record.get_field("VAL"),
        Some(EpicsValue::Double(4.5)),
        "sub's do_sub returns 0 and never assigns VAL"
    );
}
