//! R11-10 — SMOOTH/NSMOOTH PRESERVE the border elements (`aCalcPerform.c:968-975`,
//! `:578-586`); the port zeroed them.
//!
//! C's loop covers `firstEl+2 ..= lastEl-2` and writes IN PLACE, so the first two and
//! last two elements simply keep the values they had — and a window shorter than five
//! elements comes back untouched, because the loop body never runs. The port seeded a
//! zero result buffer and filled only the interior, so `SMOO(AA)` zeroed four
//! elements and `SMOO` of a short array zeroed all of them.
//!
//! Every expectation below is the output of a driver compiled from
//! `/home/stevek/work/epics-modules/calc/calcApp/src/{aCalcPerform,aCalcPostfix,calcUtil}.c`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

/// arraySize 7 with a spike at index 3.
fn spike7() -> ArrayInputs {
    let mut i = ArrayInputs::new(7);
    i.arrays[0] = vec![1.0, 2.0, 3.0, 40.0, 5.0, 6.0, 7.0];
    i
}

fn a(expr: &str, inputs: &mut ArrayInputs) -> Vec<f64> {
    match acalc(expr, inputs).expect("status 0") {
        ArrayStackValue::Array(cell) => cell.buf().to_vec(),
        other => panic!("expected an Array result, got {other:?}"),
    }
}

/// Compiled C, AA=[1,2,3,40,5,6,7]: `SMOO(AA)` = [1, 2, 12, 17.5, 14, 6, 7].
/// The four border elements are the INPUT's, not zeros.
#[test]
fn r11_10_smooth_preserves_the_border_elements() {
    let mut i = spike7();
    assert_eq!(
        a("SMOO(AA)", &mut i),
        vec![1.0, 2.0, 12.0, 17.5, 14.0, 6.0, 7.0]
    );
}

/// A window shorter than the 5-point kernel leaves the array UNCHANGED — C's loop
/// bounds (`firstEl+2 ..= lastEl-2`) cross and the body never runs. Compiled C:
/// `SMOO(AA[0,3])` = [1,2,3,40,0,0,0] (the zeros are the SUBRANGE's, not SMOOTH's),
/// and `SMOO(AA)` on a 4-element buffer = [1,2,3,40].
#[test]
fn r11_10_a_window_under_five_elements_is_unchanged() {
    let mut i = spike7();
    assert_eq!(
        a("SMOO(AA[0,3])", &mut i),
        vec![1.0, 2.0, 3.0, 40.0, 0.0, 0.0, 0.0]
    );

    let mut i = ArrayInputs::new(4);
    i.arrays[0] = vec![1.0, 2.0, 3.0, 40.0];
    assert_eq!(a("SMOO(AA)", &mut i), vec![1.0, 2.0, 3.0, 40.0]);
}

/// SMOOTH honours the operand's WINDOW (its `calcFirstLast` runs on the array) and
/// leaves everything outside it alone. Compiled C: `SMOO(AA[1,5])` = [2,3,17.5,5,6,0,0]
/// — the subrange shifted [2,3,40,5,6] down, and only its one interior element moved.
#[test]
fn r11_10_smooth_smooths_inside_the_window_only() {
    let mut i = spike7();
    assert_eq!(
        a("SMOO(AA[1,5])", &mut i),
        vec![2.0, 3.0, 17.5, 5.0, 6.0, 0.0, 0.0]
    );
}

/// NSMOOTH does NOT honour the window, and that is C's structure, not an oversight:
/// its `calcFirstLast` runs BEFORE `DEC(ps)` (`:580-582`), i.e. on the npts SCALAR,
/// whose numEl is always the -1 sentinel — so first/last come out as the whole buffer
/// whatever window the array carries. Compiled C, same array and window:
///   SMOO(AA[1,5])    -> [2, 3, 17.5, 5,       6, 0, 0]   (window only)
///   NSMOO(AA[1,5],1) -> [2, 3, 17.5, 13.5625, 6, 0, 0]   (whole buffer)
/// The difference at index 3 is the whole point: NSMOOTH smoothed a position SMOOTH
/// would not touch.
#[test]
fn r11_10_nsmooth_ignores_the_window() {
    let mut i = spike7();
    assert_eq!(
        a("NSMOO(AA[1,5],1)", &mut i),
        vec![2.0, 3.0, 17.5, 13.5625, 6.0, 0.0, 0.0]
    );
}

/// Border preservation compounds across passes: each pass reads the borders the
/// previous one kept. Compiled C: `NSMOO(AA,2)` = [1, 2, 10.3125, 13.5625, 12.3125,
/// 6, 7] — with zeroed borders the second pass would have fed on zeros and the
/// interior would be wrong too, not just the edges.
#[test]
fn r11_10_nsmooth_passes_compound_on_preserved_borders() {
    let mut i = spike7();
    assert_eq!(
        a("NSMOO(AA,2)", &mut i),
        vec![1.0, 2.0, 10.3125, 13.5625, 12.3125, 6.0, 7.0]
    );
}
