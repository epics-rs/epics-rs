//! `LALM` is armed only when `recGblSetSevr` actually RAISED the severity.
//!
//! C `aiRecord.c:404-409`:
//!
//! ```c
//! if (asev) {
//!     /* Report alarm condition, store LALM for future HYST calculations */
//!     if (recGblSetSevr(prec, range_stat[alarmRange], asev))
//!         prec->lalm = alev;
//! } else {
//!     /* No alarm condition, reset LALM */
//!     prec->lalm = val;
//! }
//! ```
//!
//! `recGblSetSevrVMsg` (`recGbl.c:237-256`) returns TRUE only at
//! `prec->nsev < new_sevr` (`:254`), FALSE otherwise (`:256`), and
//! `recGblSetSevr` (`:258-261`) passes that through. Every C ladder in base
//! and synApps gates on it — `selRecord.c:269/278/287/296`,
//! `dfanoutRecord.c:246/255/264/273`, `aCalcoutRecord.c:867/873/879/885`,
//! `sCalcoutRecord.c:727/733/739/745`, `calcRecord.c:386`,
//! `subRecord.c:338/347/356/365`, `longinRecord.c:360`,
//! `int64inRecord.c:353`, `aoRecord.c:396/405/414/423`,
//! `longoutRecord.c:330/339/348/357`, `int64outRecord.c:311/320/329/338`,
//! `calcoutRecord.c:576/585/594/603` — with no exceptions.
//!
//! Why the gate is not cosmetic: `LALM` is the hysteresis latch. Arming it on
//! a cycle whose alarm LOST the severity compare makes the next cycle's
//! `lalm == alev && val >= alev - hyst` clause hold an alarm C has already
//! cleared, so the record reports a severity C does not.
//!
//! Boundaries, one case each: the level RAISES (latch arms) / the level TIES
//! (latch must not move) / no alarm at all (latch tracks VAL, ungated in C
//! too), then the consequence of a tie carried into the hysteresis band, on
//! both the shared ladder and each record that owns its own.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{self, alarm_status};
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

/// `SRC` alarms at MAJOR on its own HIHI; `T` inherits that MAJOR through
/// `INP ... MS` before its own ladder runs (C raises it inside `dbGetLink`,
/// so `checkAlarms` sees the pending severity), which makes `T`'s own
/// HIHI/MAJOR a TIE. `P` is the same record with no MS input — its HIHI
/// raises from NO_ALARM and therefore does arm the latch.
///
/// `SRC` deliberately leaves `HYST` at 0 so it clears the moment VAL drops
/// below HIHI; `T` and `P` carry `HYST 2` because the band is what the latch
/// controls.
const DB: &str = r#"
record(ai, "SRC") { field(HIHI, "10") field(HHSV, "MAJOR") }
record(ai, "T")   { field(INP, "SRC MS")
                    field(HIHI, "10") field(HHSV, "MAJOR") field(HYST, "2") }
record(ai, "P")   { field(HIHI, "10") field(HHSV, "MAJOR") field(HYST, "2") }

record(sel,      "S:TIE") { field(SELM, "Specified") field(SELN, "0")
                            field(HIHI, "10") field(HHSV, "MAJOR") field(HYST, "2") }
record(dfanout,  "D:TIE") { field(HIHI, "10") field(HHSV, "MAJOR") field(HYST, "2") }
record(acalcout, "C:TIE") { field(HIHI, "10") field(HHSV, "MAJOR") field(HYST, "2") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn sevr(db: &PvDatabase, rec: &str) -> AlarmSeverity {
    db.get_record(rec).unwrap().read().common.sevr
}

fn lalm(db: &PvDatabase, rec: &str) -> f64 {
    let rr = db.get_record(rec).unwrap();
    let inst = rr.read();
    inst.record
        .get_field("LALM")
        .and_then(|v| v.to_f64())
        .expect("LALM")
}

/// Drive `SRC` to `v` and then process `T`, which reads it through `INP MS`.
async fn drive_through_src(db: &PvDatabase, v: f64) {
    db.put_pv("SRC", EpicsValue::Double(v)).await.unwrap();
    process(db, "SRC").await;
    process(db, "T").await;
}

/// Boundary — the level RAISES. Nothing is pending, so `recGblSetSevr`
/// returns TRUE and C arms the latch at the THRESHOLD (10), not at VAL (11).
#[epics_macros_rs::epics_test]
async fn a_level_that_raises_arms_the_latch_at_the_threshold() {
    let db = build().await;
    db.put_pv("P", EpicsValue::Double(11.0)).await.unwrap();
    process(&db, "P").await;
    assert_eq!(sevr(&db, "P"), AlarmSeverity::Major);
    assert_eq!(lalm(&db, "P"), 10.0, "a raise arms LALM at HIHI");
}

/// Boundary — the level TIES. `T` already carries MAJOR from its MS input, so
/// `recGblSetSevr(HIHI, MAJOR)` finds `nsev == new_sevr`, returns FALSE, and C
/// leaves LALM at its init value.
#[epics_macros_rs::epics_test]
async fn a_level_that_ties_leaves_the_latch_where_it_was() {
    let db = build().await;
    drive_through_src(&db, 11.0).await;
    assert_eq!(sevr(&db, "T"), AlarmSeverity::Major, "MS carries the MAJOR");
    assert_eq!(
        lalm(&db, "T"),
        0.0,
        "the HIHI level tied and must not arm the latch"
    );
}

/// The consequence of the tie, and the operational failure: next cycle the
/// upstream MAJOR is gone and VAL sits INSIDE the band (`10 - 2 <= 9 < 10`).
/// C never armed the latch, so the second clause cannot fire and the record
/// is clean. An armed latch would hold MAJOR here.
#[epics_macros_rs::epics_test]
async fn an_unarmed_latch_does_not_hold_the_alarm_into_the_band() {
    let db = build().await;
    drive_through_src(&db, 11.0).await;
    drive_through_src(&db, 9.0).await;
    assert_eq!(sevr(&db, "SRC"), AlarmSeverity::NoAlarm, "SRC HYST is 0");
    assert_eq!(
        sevr(&db, "T"),
        AlarmSeverity::NoAlarm,
        "VAL 9 is inside the band but the latch was never armed"
    );
}

/// Control for the case above — an ARMED latch must still hold the alarm
/// across the band, or the gate would have broken hysteresis itself.
#[epics_macros_rs::epics_test]
async fn an_armed_latch_holds_the_alarm_into_the_band() {
    let db = build().await;
    db.put_pv("P", EpicsValue::Double(11.0)).await.unwrap();
    process(&db, "P").await;
    db.put_pv("P", EpicsValue::Double(9.0)).await.unwrap();
    process(&db, "P").await;
    assert_eq!(sevr(&db, "P"), AlarmSeverity::Major);
    assert_eq!(lalm(&db, "P"), 10.0, "the latch stays at HIHI in the band");
}

/// Boundary — no alarm condition. C `aiRecord.c:409` writes `lalm = val`
/// UNCONDITIONALLY, so only the alarm arm is gated.
#[epics_macros_rs::epics_test]
async fn no_alarm_condition_tracks_the_latch_to_val() {
    let db = build().await;
    db.put_pv("P", EpicsValue::Double(11.0)).await.unwrap();
    process(&db, "P").await;
    db.put_pv("P", EpicsValue::Double(7.0)).await.unwrap();
    process(&db, "P").await;
    assert_eq!(sevr(&db, "P"), AlarmSeverity::NoAlarm);
    assert_eq!(lalm(&db, "P"), 7.0, "the no-alarm arm follows VAL");
}

/// The three records that own their ladder instead of using the shared one
/// (`selRecord.c:265-300`, `dfanoutRecord.c:242-277`,
/// `aCalcoutRecord.c:865-890`) obey the same rule. Raising an equal severity
/// through the public owner is exactly what an MS input link does one step
/// earlier, and needs no link plumbing to express.
#[epics_macros_rs::epics_test]
async fn a_record_owned_ladder_ties_the_same_way() {
    for rec in ["S:TIE", "D:TIE", "C:TIE"] {
        for (pending, expect) in [(AlarmSeverity::Major, 0.0), (AlarmSeverity::NoAlarm, 10.0)] {
            let db = build().await;
            let rr = db.get_record(rec).unwrap();
            let mut guard = rr.write();
            let inst = &mut *guard;
            inst.common.udf = 0;
            inst.record
                .put_field("VAL", EpicsValue::Double(11.0))
                .unwrap();
            if pending != AlarmSeverity::NoAlarm {
                recgbl::rec_gbl_set_sevr(&mut inst.common, alarm_status::LINK_ALARM, pending);
            }
            inst.record.check_alarms(&mut inst.common);
            assert_eq!(
                inst.record.get_field("LALM").and_then(|v| v.to_f64()),
                Some(expect),
                "{rec}: pending {pending:?} — LALM"
            );
        }
    }
}
