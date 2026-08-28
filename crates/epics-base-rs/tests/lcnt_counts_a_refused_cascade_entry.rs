//! A cascade entry that is refused must be COUNTED, exactly as C counts it.
//!
//! C `dbProcess` has one test for "this record is already active" — PACT — and
//! one arm behind it (`dbAccess.c:535-555` at R7.0.10):
//!
//! ```c
//! /* If already active don't process */
//! if (precord->pact) {
//!     ...
//!     /* raise scan alarm after MAX_LOCK times */
//!     if ((precord->stat == SCAN_ALARM) ||
//!         (precord->lcnt++ < MAX_LOCK) ||
//!         (precord->sevr >= INVALID_ALARM)) goto all_done;
//!
//!     recGblSetSevrMsg(precord, SCAN_ALARM, INVALID_ALARM, "Async in progress");
//! ```
//!
//! C reaches that arm on a link cascade because `processTarget` forces
//! `psrc->pact = TRUE` (`dbDbLink.c:457`) and then calls `dbProcess(pdst)`
//! UNCONDITIONALLY (`dbDbLink.c:512`) — an active destination is refused by
//! `dbProcess` itself, not by the caller.
//!
//! The port sets PACT only for an async defer, so "already active" is two
//! tests here: `is_processing()` for the async half, and the `visited` stack
//! marker for the synchronous half. Both had a way to decline without
//! counting — the prelude's cycle guard returned a bare `Ok(None)`, and
//! `process_target` gated the recursive call on `!pact` so an active target
//! never reached `dbProcess` at all.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;

async fn db_from(text: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn lcnt_of(db: &PvDatabase, rec: &str) -> i16 {
    db.get_record(rec)
        .expect("record exists")
        .read()
        .common
        .lcnt
}

fn alarm_of(db: &PvDatabase, rec: &str) -> (u16, AlarmSeverity, String) {
    let inst = db.get_record(rec).expect("record exists");
    let g = inst.read();
    (g.common.stat, g.common.sevr, g.common.amsg.clone())
}

/// Boundary: the SYNCHRONOUS half — a record already on the current process
/// stack. `A -> FLNK -> B -> FLNK -> A` re-enters A while A's own cycle is
/// running, which in C is `precord->pact` set by A's record support and again
/// by `processTarget`. One refused entry, so `lcnt_before == 0 < MAX_LOCK`:
/// counted, no alarm.
#[epics_macros_rs::epics_test]
async fn a_cycle_back_into_the_running_record_counts_in_lcnt() {
    let db = db_from(
        "record(calc, \"LCNT:A\") { field(CALC,\"A+1\") field(INPA,\"0\") field(FLNK,\"LCNT:B\") }\n\
         record(calc, \"LCNT:B\") { field(CALC,\"A+1\") field(INPA,\"0\") field(FLNK,\"LCNT:A\") }\n",
    )
    .await;

    let mut visited = HashSet::new();
    db.process_record_with_links("LCNT:A", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        lcnt_of(&db, "LCNT:A"),
        1,
        "B's FLNK re-entered A while A's cycle was on the stack; C's dbProcess \
         counts that refusal in LCNT"
    );
    assert_eq!(
        lcnt_of(&db, "LCNT:B"),
        0,
        "B ran its cycle, so C's trailing `precord->lcnt = 0` applies to it"
    );
    let (stat, sevr, amsg) = alarm_of(&db, "LCNT:A");
    assert_eq!(
        (stat, sevr, amsg.as_str()),
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm, ""),
        "one refusal is below MAX_LOCK, so C takes `goto all_done` with no alarm"
    );
}

/// Boundary: the ASYNC half reached THROUGH A LINK, and the MAX_LOCK edge.
/// C calls `dbProcess(pdst)` on an active target and lets it refuse, so the
/// count accrues across cascades; at the eleventh refusal `lcnt++` is no
/// longer `< MAX_LOCK` and SCAN_ALARM / INVALID is raised.
#[epics_macros_rs::epics_test]
async fn a_pact_link_target_accrues_lcnt_and_alarms_past_max_lock() {
    let db = db_from(
        "record(calc, \"LCNTP:SRC\") { field(CALC,\"A+1\") field(INPA,\"0\") field(FLNK,\"LCNTP:TGT\") }\n\
         record(calc, \"LCNTP:TGT\") { field(CALC,\"A+1\") field(INPA,\"0\") }\n",
    )
    .await;

    // C's guard short-circuits on `sevr >= INVALID_ALARM`, and a
    // never-processed record still carries the init UDF severity. Put the
    // target in the state a completed cycle leaves behind, then hold it
    // active the way an async device round-trip would.
    {
        let rec = db.get_record("LCNTP:TGT").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 0;
        inst.common.sevr = AlarmSeverity::NoAlarm;
        inst.common.stat = alarm_status::NO_ALARM;
        inst.enter_pact();
    }

    for i in 1..=10 {
        let mut visited = HashSet::new();
        db.process_record_with_links("LCNTP:SRC", &mut visited, 0)
            .await
            .unwrap();
        assert_eq!(
            lcnt_of(&db, "LCNTP:TGT"),
            i as i16,
            "cascade {i} must reach dbProcess on the active target and be counted"
        );
        assert_eq!(
            alarm_of(&db, "LCNTP:TGT").1,
            AlarmSeverity::NoAlarm,
            "still below MAX_LOCK at refusal {i}"
        );
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("LCNTP:SRC", &mut visited, 0)
        .await
        .unwrap();
    let (stat, sevr, amsg) = alarm_of(&db, "LCNTP:TGT");
    assert_eq!(stat, alarm_status::SCAN_ALARM);
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(amsg, "Async in progress");
}
