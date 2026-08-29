//! R18-1: the CALC assignment operator `:=` writes the record's A..U.
//!
//! C `calcPerform(&prec->a, &prec->val, rpcl)` is handed a pointer INTO the
//! record, so the store opcode IS the field write (`calcPerform.c:100-122`):
//!
//! ```c
//!     case STORE_A: … case STORE_U:
//!         parg[op - STORE_A] = *ptop--;
//! ```
//!
//! sCalc does the same for A..L and, through `psarg`, for the string args
//! AA..LL (`sCalcPerform.c:429-433`, `:888-894`). Compiled softIoc,
//! `CALC="A:=A+1;A"`, three PROCs: `VAL=1 A=1 / VAL=2 A=2 / VAL=3 A=3`.
//!
//! The port evaluated an owned COPY of the args and dropped it, so every `:=`
//! was a silent no-op: VAL climbed, A stayed 0 forever. calcout was worse — its
//! OCAL pass built a SECOND fresh copy, so CALC's stores were invisible to OCAL
//! in the same cycle where C shares one `&prec->a`.
//!
//! Boundaries: the store lands (calc/calcout/scalcout/swait/transform); it
//! accumulates across cycles; it is visible to the second pass of the same
//! cycle (calcout CALC → OCAL, transform channel → channel).

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "C:CALC") {
    field(CALC, "A:=A+1;A")
}
record(calcout, "C:OUT") {
    field(CALC, "A:=A+1;A")
    field(OCAL, "A*10")
    field(DOPT, "Use OCAL")
    field(OOPT, "Every Time")
}
record(scalcout, "C:SCALC") {
    field(CALC, "A:=A+1;AA:='hit';A")
}
record(swait, "C:SWAIT") {
    field(CALC, "A:=A+1;A")
}
record(transform, "C:XFORM") {
    field(CLCB, "A:=5;A")
    field(CLCC, "A+1")
}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

async fn f64_of(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// Compiled softIoc, `record(calc)` with `CALC="A:=A+1;A"`, 3 × PROC:
/// `VAL=1 A=1 / VAL=2 A=2 / VAL=3 A=3`.
#[epics_macros_rs::epics_test]
async fn calc_store_lands_in_a_and_accumulates() {
    let db = build().await;

    for expected in 1..=3 {
        process(&db, "C:CALC").await;
        assert_eq!(
            f64_of(&db, "C:CALC.A").await,
            expected as f64,
            "`A:=A+1` must write the record's A (C: parg[op-STORE_A] = *ptop--)"
        );
        assert_eq!(
            f64_of(&db, "C:CALC").await,
            expected as f64,
            "VAL follows A"
        );
    }
}

/// calcout shares ONE arg set between its two passes: C hands
/// `calcPerform(&prec->a, &prec->val, rpcl)` (`:238`) and
/// `calcPerform(&prec->a, &prec->oval, orpc)` (`:621`) the SAME `&prec->a`, so
/// OCAL reads the A that CALC just stored. First cycle: CALC stores A=1, VAL=1;
/// OCAL then computes OVAL = A*10 = 10 (not 0, which is what a fresh copy of
/// the pre-CALC args would give).
#[epics_macros_rs::epics_test]
async fn calcout_ocal_sees_the_calc_pass_store() {
    let db = build().await;

    process(&db, "C:OUT").await;
    assert_eq!(
        f64_of(&db, "C:OUT.A").await,
        1.0,
        "the CALC store lands in A"
    );
    assert_eq!(f64_of(&db, "C:OUT").await, 1.0, "VAL is the CALC result");
    assert_eq!(
        f64_of(&db, "C:OUT.OVAL").await,
        10.0,
        "OCAL is handed the same &prec->a, so `A*10` sees the stored A=1"
    );

    process(&db, "C:OUT").await;
    assert_eq!(f64_of(&db, "C:OUT.A").await, 2.0);
    assert_eq!(f64_of(&db, "C:OUT.OVAL").await, 20.0);
}

/// sCalc stores both families: `A:=` through `parg` and `AA:=` through `psarg`
/// (`sCalcPerform.c:888-894`, `strncpy(psarg[op - STORE_AA], ps->s, …)`).
#[epics_macros_rs::epics_test]
async fn scalcout_stores_both_the_numeric_and_the_string_arg() {
    let db = build().await;

    process(&db, "C:SCALC").await;
    assert_eq!(f64_of(&db, "C:SCALC.A").await, 1.0, "`A:=A+1` writes A");
    assert_eq!(
        db.get_pv("C:SCALC.AA").unwrap(),
        EpicsValue::String("hit".into()),
        "`AA:='hit'` writes the string arg AA (C strncpy into psarg[0])"
    );

    process(&db, "C:SCALC").await;
    assert_eq!(f64_of(&db, "C:SCALC.A").await, 2.0, "and it accumulates");
}

/// swait: C `swaitRecord.c:409` — `calcPerform(&pwait->a, &pwait->val, rpcl)`.
#[epics_macros_rs::epics_test]
async fn swait_store_lands_in_a() {
    let db = build().await;

    process(&db, "C:SWAIT").await;
    assert_eq!(f64_of(&db, "C:SWAIT.A").await, 1.0);
    process(&db, "C:SWAIT").await;
    assert_eq!(f64_of(&db, "C:SWAIT.A").await, 2.0);
}

/// transform evaluates its channels in order against ONE arg set
/// (`transformRecord.c:593`, `sCalcPerform(&ptran->a, 16, …)` per channel), so
/// channel B's `A:=5` is what channel C's `A+1` fetches in the SAME cycle.
#[epics_macros_rs::epics_test]
async fn transform_channel_store_is_visible_to_the_next_channel() {
    let db = build().await;

    process(&db, "C:XFORM").await;
    assert_eq!(
        f64_of(&db, "C:XFORM.A").await,
        5.0,
        "CLCB's `A:=5` writes the record's A"
    );
    assert_eq!(
        f64_of(&db, "C:XFORM.C").await,
        6.0,
        "CLCC's `A+1` runs against the stored A=5, not the pre-cycle A=0"
    );
}
