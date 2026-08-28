//! R10-11 — aCalc's `status` cell: who writes it, and what a write means.
//!
//! `status` (`aCalcPerform.c:422`) is read exactly once, at the end of the expression
//! (`:1591-1594`), and a non-zero value suppresses the result write entirely — the
//! record keeps its previous VAL/AVAL and raises CALC_ALARM/INVALID.
//!
//! Two things the port had wrong:
//!
//! * **the FIT/DERIV family never wrote it.** C assigns `status = deriv(...)` (`:608`,
//!   `:976`) and `status = fitpoly(...)` (`:999`, `:1020`, `:1210`, `:1259`); a fit
//!   fails on fewer than three points or a singular normal matrix (`calcUtil.c:271`,
//!   `:297`). The port silently substituted a zero curve and reported success, so a
//!   record whose window had collapsed published zeros as if they were data.
//! * **the cell was sticky.** Every C write is an ASSIGNMENT, including the `status =
//!   0` that opens each array SQRT/LOG guard (back in `aCalcPerform.c`: `:776`,
//!   `:792`, `:804`), so the LAST
//!   fallible operator decides and a clean one CLEARS an earlier failure. The port's
//!   `Option` only ever went from None to Some.
//!
//! Every expectation below is the output of a driver compiled from
//! `/home/stevek/work/epics-modules/calc/calcApp/src/{aCalcPerform,aCalcPostfix,calcUtil}.c`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

/// arraySize 6, A=5, BB=[1..6], CC all zeros — so `CC-1` is all-negative and is the
/// SQRT domain failure, and `BB[0,1]` is a two-point window and is the fit failure.
///
/// CC is seeded EXPLICITLY, because that is the shape the record hands the engine: C's
/// `aa..ll` always point at real `arraySize` buffers, never at nothing. An `arrays[i]`
/// left empty here is the port's "no such array" sentinel and fetches as the SCALAR 0,
/// which would send `CC-1` down the scalar branch and past the domain guard.
fn inputs() -> ArrayInputs {
    let mut i = ArrayInputs::new(6);
    i.num_vars[0] = 5.0;
    i.arrays[1] = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    i.arrays[2] = vec![0.0; 6];
    i
}

fn buf(v: ArrayStackValue) -> Vec<f64> {
    match v {
        ArrayStackValue::Array(cell) => cell.buf().to_vec(),
        other => panic!("expected an Array result, got {other:?}"),
    }
}

/// A window too short to fit a quadratic fails, in every one of the six operators that
/// calls `fitpoly`. Compiled C, all six: status=-1.
#[test]
fn r10_11_a_failed_fit_is_a_calc_error() {
    for expr in [
        "FITPOLY(BB[0,1])",
        "FITMPOLY(BB[0,1],CC)",
        "FITQ(BB[0,1],C,D,E)",
        "FITMQ(BB[0,1],CC,C,D,E)",
        "DERIV(BB[0,1])",
        "NDERIV(BB[0,1],1)",
    ] {
        assert!(
            acalc(expr, &mut inputs()).is_err(),
            "{expr} is C's status -1 and must not report success"
        );
    }
}

/// The same failure reached the other way: NDERIV's own npts, not the window, drives
/// `m = 2*npts+1` below three. Compiled C, `NDERIV(BB,0)`: status=-1.
#[test]
fn r10_11_nderiv_with_too_few_points_is_a_calc_error() {
    assert!(acalc("NDERIV(BB,0)", &mut inputs()).is_err());
}

/// The negative control: a window that CAN carry a quadratic must still succeed, and
/// must still return the curve. Compiled C, `FITPOLY(BB)` on the line [1..6]: status=0,
/// aresult=[1,2,3,4,5,6]; `DERIV(BB)`: status=0, aresult all 1.
#[test]
fn r10_11_a_successful_fit_is_not_an_error() {
    assert_eq!(
        buf(acalc("FITPOLY(BB)", &mut inputs()).expect("a 6-point window fits")),
        vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]
    );
    for (i, &v) in buf(acalc("DERIV(BB)", &mut inputs()).expect("a 6-point window fits"))
        .iter()
        .enumerate()
    {
        assert!((v - 1.0).abs() < 1e-9, "d/di of a unit ramp at {i} is {v}");
    }
}

/// The cell is LAST-WRITER-WINS, not sticky: the array SQRT guard opens with
/// `status = 0` (`:776`), so a clean SQRT after a failed one clears the failure — and
/// the failed one's zeroed elements are still in the arithmetic. Compiled C,
/// `SQRT(CC-1)+SQRT(BB)`: status=0, aresult=[1, 1.414…, 1.732…, 2, 2.236…, 2.449…].
#[test]
fn r10_11_a_clean_domain_guard_clears_an_earlier_failure() {
    let got = buf(acalc("SQRT(CC-1)+SQRT(BB)", &mut inputs()).expect("C reports status 0 here"));
    for (i, &v) in got.iter().enumerate() {
        let want = (i as f64 + 1.0).sqrt();
        assert!((v - want).abs() < 1e-9, "element {i} is {v}, want {want}");
    }
}

/// The negative control for that, and the reason the order matters: reverse the two
/// operands and the FAILING guard is the last writer. Compiled C,
/// `SQRT(BB)+SQRT(CC-1)`: status=-1.
#[test]
fn r10_11_the_last_fallible_operator_decides() {
    assert!(
        acalc("SQRT(BB)+SQRT(CC-1)", &mut inputs()).is_err(),
        "the failing SQRT runs second and its -1 must stand"
    );
}

/// The clearing crosses operator families, because there is only ONE cell: a
/// successful fit clears a failed domain guard, and a clean domain guard clears a
/// failed fit. Compiled C: `SQRT(CC-1)+FITPOLY(BB)` -> status=0;
/// `DERIV(BB[0,1])+SQRT(BB)` -> status=0.
#[test]
fn r10_11_the_status_cell_is_shared_by_both_families() {
    assert!(acalc("SQRT(CC-1)+FITPOLY(BB)", &mut inputs()).is_ok());
    assert!(acalc("DERIV(BB[0,1])+SQRT(BB)", &mut inputs()).is_ok());
}

/// The SCALAR branch of the unary switch writes no status at all — not even to clear
/// it (`:1033-1090` has no `status =` anywhere). So a scalar SQRT/DERIV/FITPOLY after
/// a failed array operator leaves the failure standing. Compiled C, all three:
/// status=-1.
#[test]
fn r10_11_the_scalar_branch_does_not_touch_the_status() {
    for expr in [
        "SQRT(CC-1)+SQRT(A)",
        "SQRT(CC-1)+DERIV(A)",
        "SQRT(CC-1)+FITPOLY(A)",
    ] {
        assert!(
            acalc(expr, &mut inputs()).is_err(),
            "{expr}: a scalar operand cannot clear an array operator's -1"
        );
    }
}
