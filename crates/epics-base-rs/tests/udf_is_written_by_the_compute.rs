//! **`udf` MUST be written only by the step that produced the value.**
//!
//! C never re-derives `prec->udf` per cycle for these four record types: the
//! assignment sits inside the compute's success arm, so a cycle that ran no
//! compute leaves UDF at its previous value.
//!
//! ```c
//! /* subRecord.c:426-436 */   if (psubroutine == NULL) { recGblSetSevr(BAD_SUB_ALARM, INVALID); return 0; }
//!                             status = (*psubroutine)(prec);
//!                             if (status < 0) recGblSetSevr(SOFT_ALARM, prec->brsv);
//!                             else            prec->udf = isnan(prec->val);
//! /* calcRecord.c:120-125 */  if (fetch_values(prec) == 0) {
//!                                 if (calcPerform(...)) recGblSetSevr(CALC_ALARM, INVALID);
//!                                 else                  prec->udf = isnan(prec->val);
//!                             }
//! /* calcoutRecord.c:237-243, 620-625 */  same shape, twice (VAL and OVAL)
//! /* selRecord.c:397-402 */   default: recGblSetSevr(CALC_ALARM, INVALID); return;
//!                             }
//!                             prec->val = val;
//!                             prec->udf = isnan(prec->val);
//! ```
//!
//! The port hoisted that write into a per-cycle blanket
//! (`processing.rs` / `record_instance.rs`: `common.udf =
//! record.value_is_undefined()`, gated on [`Record::clears_udf`]), so UDF meant
//! "the value looks defined" on the blanket path and "the record produced a
//! value" on C's. GROUND TRUTH — built C `softIoc` (7.0.10.1-DEV) and
//! `softioc-rs`, same `.db`, each record processed once by `dbpf <rec>.PROC 1`:
//!
//! ```text
//! S1.UDF  1   S1.STAT  BAD_SUB   (sub, SNAM names no registered function)
//! C1.UDF  1   C1.STAT  LINK      (calc,    INPA read failed)
//! CO1.UDF 1   CO1.STAT LINK      (calcout, INPA read failed)
//! SL1.UDF 1   SL1.STAT LINK      (sel,     INPA read failed)
//! C2.UDF  0   CO2.UDF 0   SL2.UDF 0        (a compute that ran and defined VAL)
//! C3.UDF  1   C3.STAT  UDF       (calc CALC="0/0" — the compute ran, VAL is NaN)
//! SL3.UDF 1   SL3.STAT SOFT      (sel SELN=99 — do_sel returns before the write)
//! ```
//!
//! The port reported `0` for the first four before this fix.
//!
//! The cases below are one per invariant boundary, not one per story: the gate
//! open vs. closed, the compute succeeding vs. failing, and the value defined
//! vs. NaN on the arm that does write.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{CommonFields, Record};
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::sel::SelRecord;
use epics_base_rs::types::EpicsValue;

/// A loaded, never-processed record: UDF set, which is the value every case
/// below either preserves or replaces.
fn undefined() -> CommonFields {
    let common = CommonFields::default();
    assert_eq!(common.udf, 1, "a loaded record starts undefined");
    common
}

fn with_calc(rec: &mut dyn Record, field: &str, expr: &str) {
    rec.put_field(field, EpicsValue::String(expr.into()))
        .unwrap();
    rec.special(field, true).unwrap();
}

/// One cycle of the record's own half of C `process()` — the compute, then the
/// `checkAlarms` hook that owns `CommonFields`. The framework's blanket
/// re-derive is deliberately NOT run here: [`Record::clears_udf`] is what
/// decides whether it would, and each case asserts that too.
fn cycle(rec: &mut dyn Record, common: &mut CommonFields) {
    assert!(
        !rec.clears_udf(),
        "{} must own its UDF write; the per-cycle blanket re-derive would \
         overwrite it on a cycle that computed nothing",
        rec.record_type()
    );
    rec.process().unwrap();
    rec.check_alarms(common);
}

/// calc: the `fetch_values` gate. C runs no `calcPerform` and writes no UDF.
#[test]
fn a_failed_input_fetch_freezes_calc_udf() {
    let mut rec = CalcRecord::default();
    with_calc(&mut rec, "CALC", "1");
    let mut common = undefined();

    rec.set_fetch_gate_failed(true);
    cycle(&mut rec, &mut common);
    assert_eq!(common.udf, 1, "C leaves UDF alone when fetch_values fails");

    // Gate open: the framework pushes the outcome every cycle, so the same
    // record on the next cycle computes and defines itself.
    rec.set_fetch_gate_failed(false);
    cycle(&mut rec, &mut common);
    assert_eq!(common.udf, 0, "a successful calcPerform defines the record");
}

/// calc: the compute ran and produced NaN. C's `isnan(prec->val)` is a derive,
/// not a clear — the arm that writes UDF can write a 1.
#[test]
fn a_nan_calc_result_is_written_as_undefined() {
    let mut rec = CalcRecord::default();
    with_calc(&mut rec, "CALC", "0/0");
    let mut common = CommonFields {
        udf: 0,
        ..Default::default()
    };

    cycle(&mut rec, &mut common);
    assert_eq!(common.udf, 1, "CALC=0/0 computes a NaN VAL: UDF back to 1");
}

/// calcout: the same gate, and the second write site. C's `execOutput` writes
/// UDF from OVAL on a DOPT=Use_OVAL output cycle (`:624`) — reached even when
/// the CALC pass was gated out, because the OOPT switch sits outside the gate.
#[test]
fn a_failed_input_fetch_freezes_calcout_udf() {
    let mut rec = CalcoutRecord::default();
    with_calc(&mut rec, "CALC", "1");
    rec.oopt = 1; // On Change — nothing changed, so no output cycle either.
    let mut common = undefined();

    rec.set_fetch_gate_failed(true);
    cycle(&mut rec, &mut common);
    assert_eq!(common.udf, 1, "C leaves UDF alone when fetch_values fails");

    rec.set_fetch_gate_failed(false);
    cycle(&mut rec, &mut common);
    assert_eq!(common.udf, 0, "a successful calcPerform defines the record");
}

/// sel: the `fetch_values` gate wraps the whole of `do_sel` (`:114-116`).
#[test]
fn a_failed_input_fetch_freezes_sel_udf() {
    let mut rec = SelRecord::default();
    rec.put_field("INPA", EpicsValue::String("7".into()))
        .unwrap();
    rec.a = 7.0;
    let mut common = undefined();

    rec.set_fetch_gate_failed(true);
    cycle(&mut rec, &mut common);
    assert_eq!(common.udf, 1, "C leaves UDF alone when fetch_values fails");

    cycle(&mut rec, &mut common);
    assert_eq!(common.udf, 0, "the selection defined VAL");
}

/// sel: the other early exit — an out-of-range SELN in Specified mode returns
/// from `do_sel` before `prec->val = val; prec->udf = isnan(prec->val)`.
/// Measured on both binaries: `SL3.UDF 1`, `SL3.STAT SOFT`.
#[test]
fn an_out_of_range_seln_freezes_sel_udf() {
    let mut rec = SelRecord::default();
    rec.a = 7.0;
    rec.put_field("SELN", EpicsValue::UShort(99)).unwrap();
    let mut common = undefined();

    cycle(&mut rec, &mut common);
    assert_eq!(
        common.udf, 1,
        "do_sel returned before its VAL/UDF write, so UDF stands"
    );
}

/// sub: the reported symptom, end to end. A SNAM naming no registered function
/// takes `do_sub`'s `BAD_SUB_ALARM` return (`subRecord.c:426-429`), which
/// writes no UDF — and the blanket then cleared it, so a client reading `.UDF`
/// saw a never-run record as defined.
#[epics_macros_rs::epics_test]
async fn an_unresolved_snam_leaves_the_sub_record_undefined() {
    let db = PvDatabase::new();
    let mut rec = epics_base_rs::server::db_loader::create_record("sub").expect("sub record type");
    rec.put_field("SNAM", EpicsValue::String("noSuchSubroutine".into()))
        .unwrap();
    db.add_record("S1", rec).await.unwrap();

    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("S1", &mut visited, 0)
        .await
        .unwrap();

    let inst = db.get_record("S1").unwrap();
    let inst = inst.read();
    assert_eq!(
        inst.common.udf, 1,
        "C reports S1.UDF 1 — do_sub returned at BAD_SUB_ALARM without \
         reaching `prec->udf = isnan(prec->val)`"
    );
    assert_eq!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::BAD_SUB_ALARM,
        "C reports S1.STAT BAD_SUB"
    );
}
