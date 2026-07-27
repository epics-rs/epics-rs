//! A link-chain bound the port has and C does not must not read as success.
//!
//! C has no depth counter. `processTarget` (`dbDbLink.c:427-436`) marks the
//! source active and recurses; the only thing that stops it is a record that is
//! already `pact`:
//!
//! ```c
//! static long processTarget(dbCommon *psrc, dbCommon *pdst)
//! {
//!     ...
//!     psrc->pact = TRUE;
//!     ...
//! ```
//!
//! The port keeps `MAX_LINK_DEPTH` / `MAX_LINK_OPS` because every link level is
//! a `Pin<Box<dyn Future>>` driven on the calling thread's stack, and an
//! unbounded chain on an embedded target is a stack overflow rather than a slow
//! IOC. Keeping the bound is a deliberate deviation; keeping it SILENT was the
//! defect — the bail returned `Ok(None)`, so the CA `WRITE_NOTIFY` that drove
//! the chain completed with no alarm anywhere, and the only notice was an
//! `eprintln!` that reaches no errlog and no IOC log file.
//!
//! The refusal C DOES have shows what a refused cycle must look like
//! (`dbAccess.c:544-556`):
//!
//! ```c
//! recGblSetSevrMsg(precord, SCAN_ALARM, INVALID_ALARM, "Async in progress");
//! monitor_mask = recGblResetAlarms(precord);
//! monitor_mask |= DBE_VALUE|DBE_LOG;
//! db_post_events(precord, ((char *)precord) + pdbFldDes->offset, monitor_mask);
//! ```
//!
//! so the port's own bounds now land there too: SCAN_ALARM / INVALID on the
//! record that could not be processed, the reason in `AMSG`, and an `errlog`
//! line.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;

/// `MAX_LINK_DEPTH` in `database::processing`. Not importable (it is a private
/// const inside the prelude), so the chain is simply built long enough that the
/// bound is crossed wherever it sits below 32.
const CHAIN: usize = 24;

async fn chain_db() -> Arc<PvDatabase> {
    let mut db_text = String::new();
    for i in 0..CHAIN {
        let flnk = if i + 1 < CHAIN {
            format!("field(FLNK,\"L{}\")", i + 1)
        } else {
            String::new()
        };
        db_text.push_str(&format!(
            "record(calc, \"L{i}\") {{ field(CALC,\"A+1\") field(INPA,\"{i}\") {flnk} }}\n"
        ));
    }
    IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn alarm_of(db: &PvDatabase, rec: &str) -> (u16, AlarmSeverity, String) {
    let inst = db.get_record(rec).expect("record exists");
    let g = inst.read();
    (g.common.stat, g.common.sevr, g.common.amsg.clone())
}

/// The boundary itself: the deepest record the chain reaches is processed and
/// carries no refusal, while the FIRST record past the bound carries
/// SCAN_ALARM / INVALID naming the limit.
#[epics_macros_rs::epics_test]
async fn a_chain_past_the_depth_bound_leaves_scan_alarm_on_the_refused_record() {
    let db = chain_db().await;
    let mut visited = HashSet::new();
    db.process_record_with_links("L0", &mut visited, 0)
        .await
        .expect("the head processes; the bound is not an error at the head");

    // Exactly one record is refused: the first one the recursion could not
    // enter. Everything shallower ran, everything deeper was never reached.
    let refused: Vec<usize> = (0..CHAIN)
        .filter(|i| alarm_of(&db, &format!("L{i}")).0 == alarm_status::SCAN_ALARM)
        .collect();
    assert_eq!(
        refused.len(),
        1,
        "one refusal, on the record at the bound: {refused:?}"
    );
    let at = refused[0];
    assert!(
        (1..CHAIN).contains(&at),
        "the refusal must land inside the chain, not on its head"
    );

    let (stat, sevr, amsg) = alarm_of(&db, &format!("L{at}"));
    assert_eq!(stat, alarm_status::SCAN_ALARM);
    assert_eq!(
        sevr,
        AlarmSeverity::Invalid,
        "C's refused cycle is INVALID_ALARM (dbAccess.c:548)"
    );
    assert!(
        amsg.contains("link chain depth limit"),
        "the reason must reach the operator through AMSG, got {amsg:?}"
    );

    // The record below the bound ran normally: a refusal is not allowed to
    // spread to the part of the chain that did execute.
    let (below_stat, below_sevr, _) = alarm_of(&db, &format!("L{}", at - 1));
    assert_ne!(below_stat, alarm_status::SCAN_ALARM);
    assert_ne!(below_sevr, AlarmSeverity::Invalid);
}

/// The other side of the boundary: a chain that FITS raises nothing. Without
/// this the alarm could be unconditional and the first test would still pass.
#[epics_macros_rs::epics_test]
async fn a_chain_inside_the_depth_bound_raises_no_alarm() {
    let mut db_text = String::new();
    for i in 0..4 {
        let flnk = if i + 1 < 4 {
            format!("field(FLNK,\"S{}\")", i + 1)
        } else {
            String::new()
        };
        db_text.push_str(&format!(
            "record(calc, \"S{i}\") {{ field(CALC,\"A+1\") field(INPA,\"{i}\") {flnk} }}\n"
        ));
    }
    let db = IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut visited = HashSet::new();
    db.process_record_with_links("S0", &mut visited, 0)
        .await
        .unwrap();
    for i in 0..4 {
        let (stat, sevr, amsg) = alarm_of(&db, &format!("S{i}"));
        assert_ne!(stat, alarm_status::SCAN_ALARM, "S{i} must not be refused");
        assert_ne!(sevr, AlarmSeverity::Invalid, "S{i}");
        assert!(amsg.is_empty(), "S{i} amsg {amsg:?}");
    }
}

/// The distinct case that stays silent: a genuine cycle. C stops one with
/// `psrc->pact = TRUE` and returns 0 — no alarm until `MAX_LOCK` re-entries —
/// so re-reaching a record inside one cascade must not be reported as a
/// refusal.
#[epics_macros_rs::epics_test]
async fn a_cycle_is_stopped_without_an_alarm() {
    let db_text = "record(calc, \"C0\") { field(CALC,\"A+1\") field(FLNK,\"C1\") }\n\
                   record(calc, \"C1\") { field(CALC,\"A+1\") field(FLNK,\"C0\") }\n";
    let db = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut visited = HashSet::new();
    db.process_record_with_links("C0", &mut visited, 0)
        .await
        .unwrap();
    for r in ["C0", "C1"] {
        let (stat, sevr, amsg) = alarm_of(&db, r);
        assert_ne!(
            stat,
            alarm_status::SCAN_ALARM,
            "{r}: a cycle is not a bound refusal"
        );
        assert_ne!(sevr, AlarmSeverity::Invalid, "{r}");
        assert!(amsg.is_empty(), "{r} amsg {amsg:?}");
    }
}
