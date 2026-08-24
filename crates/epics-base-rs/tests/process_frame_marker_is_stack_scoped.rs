//! The cycle marker is scoped to the process frame, not to the cascade.
//!
//! C has no visited set. Its two re-entry guards are both properties of the
//! CURRENT stack: `dbProcess` refuses a record whose `pact` is set
//! (`dbAccess.c:537-557`), and `processTarget` claims `dbRec2Pvt(pdst)
//! ->procThread` immediately before `dbProcess(pdst)` and clears it
//! immediately after (`dbDbLink.c:439-440`, `:502-526`). A record reached
//! twice in one cascade — the second time after the first visit has already
//! unwound — is processed twice, and C's `claim_dst` is true again by then.
//!
//! The port's `visited` set was inserted-into and never removed, so it meant
//! "processed somewhere in this cascade". These cases pin the four boundaries
//! that distinguish the two meanings.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

fn val(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name)
        .unwrap_or_else(|e| panic!("{name} exists: {e}"))
        .to_f64()
        .unwrap_or_else(|| panic!("{name} has a numeric VAL"))
}

/// BOUNDARY (a): a genuine cycle. `L1 -FLNK-> L2 -FLNK-> L1` must terminate,
/// and the record already on the stack must run exactly once — C's
/// `dbProcess` finds `L1->pact` still TRUE (the record sets it for the
/// duration of its own `process`, and `recGblFwdLink` runs inside that) and
/// returns above the record support (`dbAccess.c:537`).
///
/// This is the boundary the removal on unwind must not regress: it is the
/// marker's presence WHILE the frame is live that bounds the loop.
#[epics_macros_rs::epics_test]
async fn a_cycle_runs_each_record_once_and_terminates() {
    let db = ioc(r#"
record(calc, "L1") { field(CALC, "VAL+1") field(FLNK, "L2") }
record(calc, "L2") { field(CALC, "VAL+1") field(FLNK, "L1") }
"#)
    .await;

    let done = epics_base_rs::runtime::task::timeout(
        Duration::from_secs(5),
        db.put_record_field_from_ca("L1", "PROC", EpicsValue::Long(1)),
    )
    .await;
    done.expect("a cycle must terminate, not recurse forever")
        .expect("the put itself succeeds");

    assert_eq!(val(&db, "L1"), 1.0, "L1 is on the stack when L2 links back");
    assert_eq!(val(&db, "L2"), 1.0);
}

/// BOUNDARY (b): a diamond. `F` fans out to `A` and `B`, both of which
/// forward-link to `C`. `A`'s frame has unwound by the time `B` runs, so C
/// processes `C` twice — verified on softIoc 7.0.10, which prints `C = 2`.
///
/// The pre-fix chain-wide marker made this 1: `C` was still in `visited` from
/// `A`'s subtree, so `B`'s forward link was refused.
#[epics_macros_rs::epics_test]
async fn a_diamond_processes_the_join_once_per_branch() {
    let db = ioc(r#"
record(fanout, "F") { field(SELM, "All") field(LNK0, "A") field(LNK1, "B") }
record(calc, "A") { field(CALC, "0") field(FLNK, "C") }
record(calc, "B") { field(CALC, "0") field(FLNK, "C") }
record(calc, "C") { field(CALC, "VAL+1") field(VAL, "0") }
"#)
    .await;
    let before = val(&db, "C");

    db.put_record_field_from_ca("F", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();

    assert_eq!(
        val(&db, "C") - before,
        2.0,
        "both fanout branches reach C, and neither is on the other's stack"
    );
}

/// BOUNDARY (c): re-entry into a record that is genuinely busy. `T` is held
/// PACT (an async cycle in flight), so the forward link must be refused by the
/// PACT gate — not by the marker, which is empty for `T` at that moment. C:
/// `dbProcess` returns at `dbAccess.c:537` without running record support.
#[epics_macros_rs::epics_test]
async fn a_pact_target_is_refused_by_the_pact_gate() {
    let db = ioc(r#"
record(calc, "TRIG") { field(CALC, "0") field(FLNK, "T") }
record(calc, "T") { field(CALC, "VAL+1") field(VAL, "0") }
"#)
    .await;
    db.get_record("T").unwrap().write().enter_pact();
    let before = val(&db, "T");

    db.put_record_field_from_ca("TRIG", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();

    assert_eq!(
        val(&db, "T"),
        before,
        "a busy record is not processed by a link that reaches it"
    );
}

/// BOUNDARY (d): the RPRO half of the same marker. `A` starts `T`'s async
/// cycle (`ODLY` keeps it PACT), `A` unwinds, and `B` then reaches the busy
/// `T` through its own forward link. C's `claim_dst` reads
/// `procThread == NULL` — cleared when `A`'s frame unwound — so the
/// `psrc->putf && claim_dst` arm fires and `T.RPRO` is set
/// (`dbDbLink.c:474-489`).
///
/// The pre-fix marker still held `T` from `A`'s subtree, so this arm was dead:
/// a put that arrived while an async record was busy was silently dropped
/// instead of being replayed on completion.
#[epics_macros_rs::epics_test]
async fn a_busy_target_reached_after_unwinding_is_marked_for_reprocessing() {
    let db = ioc(r#"
record(fanout, "F") { field(SELM, "All") field(LNK0, "A") field(LNK1, "B") }
record(calc, "A") { field(CALC, "0") field(FLNK, "T") }
record(calc, "B") { field(CALC, "0") field(FLNK, "T") }
record(calcout, "T") { field(CALC, "1") field(ODLY, "100") }
"#)
    .await;

    db.put_record_field_from_ca("F", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();

    let t = db.get_record("T").unwrap();
    let t = t.read();
    assert!(
        t.is_processing(),
        "ODLY=100 keeps T's async cycle in flight for the whole test"
    );
    assert_eq!(
        t.common.rpro, 1,
        "B reaches a busy T that is no longer on any frame: C sets RPRO"
    );
    assert!(!t.common.putf, "and clears PUTF, as C does on the same arm");
}
