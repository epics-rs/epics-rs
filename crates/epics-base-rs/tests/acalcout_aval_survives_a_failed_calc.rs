//! A failed expression leaves the acalcout record's AVAL where it was.
//!
//! C hands `aCalcPerform` the record's OWN result fields —
//! `aCalcPerform(&pcalc->a, …, &pcalc->val, pcalc->aval, …)`
//! (`aCalcoutRecord.c:1283-1285`) — so the only thing standing between a failed
//! run and the record's arrays is where `aCalcPerform` returns. It returns at
//! `aCalcPerform.c:1602-1605`:
//!
//! ```c
//!     if (status) {
//!         freeStack(flp, stack);
//!         return(status);
//!     }
//! ```
//!
//! which is ABOVE the two `p_aresult[i] = ps->a[i]` copies (`:1625`, `:1632`)
//! and above `*p_dresult` (`:1611`, `:1625`). One rule, and these are its
//! boundaries — written per boundary, because the interesting one is the fourth,
//! where CSTAT is -1 and AVAL is written anyway.
//!
//! The failure used is a fit window too short to carry a quadratic:
//! `fitpoly` returns -1 for `n < 3` (`calcUtil.c:271`), reaching
//! `status = fitpoly(...)` at `aCalcPerform.c:1008`. `calc_array_status.rs`
//! already covers that at the ENGINE boundary; nothing covered what the RECORD
//! does with AVAL afterwards.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// C's status-0 fit of `y = [1..6]` against `x = [0..5]`, i.e. the line itself.
const FITTED: [f64; 6] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
/// The AVAL every case starts from. Deliberately NOT the fitted curve: if the
/// priming value and the success value were equal, "AVAL was retained" and
/// "AVAL was recomputed" would be the same assertion.
const PRIMED: [f64; 6] = [10.0, 20.0, 30.0, 40.0, 50.0, 60.0];

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).unwrap()
}

async fn aval(db: &PvDatabase, rec: &str) -> Vec<f64> {
    match field(db, rec, "AVAL").await {
        EpicsValue::DoubleArray(v) => v,
        other => panic!("AVAL: {other:?}"),
    }
}

/// A 6-element acalcout whose AVAL has already been filled by a SUCCEEDING
/// pass, so "the previous value" is a value and not the default.
async fn primed(db: &PvDatabase, name: &str) {
    let mut a = AcalcoutRecord::new();
    a.put_field("NELM", EpicsValue::ULong(6)).unwrap();
    a.put_field("CALC", EpicsValue::String("BB*10".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field(
        "BB",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
    )
    .unwrap();
    db.add_record(name, Box::new(a)).await.unwrap();

    process(db, name).await;
    assert_eq!(aval(db, name).await, PRIMED, "the priming pass fills AVAL");
}

/// Recompile CALC on a live record, as a `caput` to the field would.
async fn recalc(db: &PvDatabase, name: &str, calc: &str) {
    let inst = db.get_record(name).unwrap();
    let mut g = inst.write();
    g.record
        .put_field("CALC", EpicsValue::String(calc.into()))
        .unwrap();
    g.record.special("CALC", true).unwrap();
}

/// Boundary 1 — `status != 0`: the return at `:1591` is taken, so neither
/// `p_aresult` nor `p_dresult` is written and both keep the previous pass's
/// values.
#[epics_macros_rs::epics_test]
async fn a_failed_fit_leaves_aval_at_its_previous_value() {
    let db = PvDatabase::new();
    primed(&db, "F1").await;
    let val_before = field(&db, "F1", "VAL").await.to_f64().unwrap();

    recalc(&db, "F1", "FITPOLY(BB[0,1])").await;
    process(&db, "F1").await;

    assert_eq!(
        aval(&db, "F1").await,
        PRIMED,
        "aCalcPerform.c:1602 returns above the p_aresult copy at :1625"
    );
    assert_eq!(
        field(&db, "F1", "VAL").await.to_f64().unwrap(),
        val_before,
        "the same return is above *p_dresult at :1611"
    );
    assert_eq!(
        field(&db, "F1", "CSTAT").await.to_f64().unwrap(),
        -1.0,
        "fitpoly's n<3 (calcUtil.c:271) is C's -1"
    );
}

/// Boundary 2 — `status == 0`, the other side of the same gate: a window that
/// CAN carry a quadratic reaches the copy, so AVAL moves.
#[epics_macros_rs::epics_test]
async fn a_fit_that_succeeds_does_write_aval() {
    let db = PvDatabase::new();
    primed(&db, "F2").await;

    recalc(&db, "F2", "FITPOLY(BB)").await;
    process(&db, "F2").await;

    assert_eq!(
        aval(&db, "F2").await,
        FITTED,
        "a 6-point window fits, so the copy at :1625 runs"
    );
    assert_eq!(field(&db, "F2", "CSTAT").await.to_f64().unwrap(), 0.0);
}

/// Boundary 3 — the retention is NOT a rollback. The gate is on the result
/// copy alone, and a store sequenced before the failure has already written the
/// record in place (`aCalcPerform.c:456-491`), so it survives while AVAL does
/// not move.
#[epics_macros_rs::epics_test]
async fn a_store_before_the_failed_fit_lands_while_aval_holds() {
    let db = PvDatabase::new();
    primed(&db, "F3").await;

    recalc(&db, "F3", "CC:=BB*2;FITPOLY(BB[0,1])").await;
    process(&db, "F3").await;

    assert_eq!(
        field(&db, "F3", "CC").await,
        EpicsValue::DoubleArray(vec![2.0, 4.0, 6.0, 8.0, 10.0, 12.0]),
        "the store landed before the fit failed and is never rolled back"
    );
    assert_eq!(
        aval(&db, "F3").await,
        PRIMED,
        "the store surviving does not mean the result was copied"
    );
    assert_eq!(
        field(&db, "F3", "AMASK").await,
        EpicsValue::ULong(0x4),
        "bit 2 = CC"
    );
}

/// Boundary 4 — `status == 0` with a non-finite result. C's `-1` here comes
/// from the TAIL, `return(isnan(*p_dresult)||isinf(*p_dresult) ? -1 : 0)`
/// (`:1633`), which is BELOW the copy — so this is the one failed calc that
/// does write AVAL. `EXP` carries no domain guard (`:1043-1045`, `:781`), and
/// `to_array(…, setValues=1)` broadcasts a non-NaN scalar across the buffer
/// (`:134-138`).
#[epics_macros_rs::epics_test]
async fn a_non_finite_result_still_writes_aval_and_still_reports_cstat() {
    let db = PvDatabase::new();
    primed(&db, "F4").await;

    recalc(&db, "F4", "EXP(1000)").await;
    process(&db, "F4").await;

    let got = aval(&db, "F4").await;
    assert!(
        got.iter().all(|v| v.is_infinite() && v.is_sign_positive()),
        "the copy at :1625 ran before the isinf test at :1633, got {got:?}"
    );
    assert_eq!(
        field(&db, "F4", "CSTAT").await.to_f64().unwrap(),
        -1.0,
        "the tail still reports the failure"
    );
}
