//! R11-8 — aCalc's DERIV/NDERIV are a sliding QUADRATIC fit (`calcUtil.c:27-74`),
//! and NDERIV's argument is points PER SIDE.
//!
//! ```c
//! m = 2*npts+1;
//! e = fitpoly(x, y, m, &c, &b, &a, NULL);        /* y = c + b*x + a*x*x */
//! for (j=0; j<m/2+1; j++) d[j] = b + 2*a*x[j];   /* dy/dx = b + 2*a*x   */
//! ...
//! int deriv(...) { return nderiv(x, y, n, d, 2, work); }
//! ```
//!
//! The port had a central difference for DERIV, a least-squares LINE for NDERIV, and
//! read NDERIV's argument as a total window width — so `NDERIV(AA,2)` fitted 2 points
//! where C fits 5, and no expression agreed with C except on a straight line.
//!
//! Every expectation below is the output of a driver compiled from
//! `/home/stevek/work/epics-modules/calc/calcApp/src/{aCalcPerform,aCalcPostfix,calcUtil}.c`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, CalcError, acalc};

/// A deliberately non-polynomial y, so every block of the sliding fit is observable.
fn noisy9() -> ArrayInputs {
    let mut i = ArrayInputs::new(9);
    i.arrays[0] = vec![0.0, 1.0, 10.0, 3.0, 4.0, 5.0, 6.0, 7.0, 20.0];
    i
}

fn a(expr: &str, inputs: &mut ArrayInputs) -> Vec<f64> {
    match acalc(expr, inputs).expect("status 0") {
        ArrayStackValue::Array(cell) => cell.buf().to_vec(),
        other => panic!("expected an Array result, got {other:?}"),
    }
}

fn close(got: &[f64], want: &[f64]) {
    assert_eq!(got.len(), want.len(), "got {got:?}, want {want:?}");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(
            (g - w).abs() < 1e-6,
            "element {i}: got {got:?}, want {want:?}"
        );
    }
}

/// Compiled C, arraySize 9, AA=[0,1,10,3,4,5,6,7,20]:
///   DERIV(AA) = 5.571428571 3.285714286 1 0.2 -0.6 1 3.4 6.828571429 10.25714286
///
/// The first three come from ONE quadratic fitted to points 0..4, the last three from
/// ONE fitted to points 4..8, and each of the middle three from its own centred fit.
/// A central difference would answer 1, 5, 1, -3, 1, 1, 1, 7, 13 — right nowhere.
#[test]
fn r11_8_deriv_is_a_sliding_quadratic_fit() {
    let mut i = noisy9();
    close(
        &a("DERIV(AA)", &mut i),
        &[
            5.571_428_571_4,
            3.285_714_285_7,
            1.0,
            0.2,
            -0.6,
            1.0,
            3.4,
            6.828_571_428_6,
            10.257_142_857_1,
        ],
    );
}

/// C `calcUtil.c:73` — `deriv()` IS `nderiv(..., npts=2, ...)`, so the two must agree
/// element for element. This is the identity the port's two separate algorithms broke.
#[test]
fn r11_8_deriv_equals_nderiv_with_two_points_per_side() {
    let mut i = noisy9();
    let d = a("DERIV(AA)", &mut i);
    let nd = a("NDERIV(AA,2)", &mut i);
    close(&d, &nd);

    // Negative control: npts is points per SIDE, so 1 and 3 are DIFFERENT operators,
    // not different window widths around the same one. Compiled C:
    //   NDERIV(AA,1) = -3 5 1 -3 1 1 1 7 19
    close(
        &a("NDERIV(AA,1)", &mut i),
        &[-3.0, 5.0, 1.0, -3.0, 1.0, 1.0, 1.0, 7.0, 19.0],
    );
    assert_ne!(a("NDERIV(AA,1)", &mut i), d);
}

/// Compiled C: NDERIV(AA,3) = 2.428571429 1.857142857 1.285714286 0.7142857143
/// 0.4285714286 1.428571429 3.80952381 6.19047619 8.571428571 — a 7-point fit
/// (m = 2*3+1), which leaves only one middle point.
#[test]
fn r11_8_nderiv_argument_is_points_per_side() {
    let mut i = noisy9();
    close(
        &a("NDERIV(AA,3)", &mut i),
        &[
            2.428_571_428_6,
            1.857_142_857_1,
            1.285_714_285_7,
            0.714_285_714_3,
            0.428_571_428_6,
            1.428_571_428_6,
            3.809_523_809_5,
            6.190_476_190_5,
            8.571_428_571_4,
        ],
    );
}

/// C clamps npts to half the window (`aCalcPerform.c:601`), so an over-long request
/// collapses onto a single fit over everything. Compiled C: `NDERIV(AA,100)` and
/// `NDERIV(AA,4)` are the same nine numbers, beginning -0.8216450216.
#[test]
fn r11_8_nderiv_clamps_npts_to_half_the_window() {
    let mut i = noisy9();
    let wide = a("NDERIV(AA,100)", &mut i);
    close(&wide, &a("NDERIV(AA,4)", &mut i));
    assert!(
        (wide[0] + 0.821_645_021_6).abs() < 1e-6,
        "one fit over all nine points: {wide:?}"
    );
}

/// A perfect quadratic is reproduced EXACTLY, at the borders too. Compiled C,
/// AA = i^2 over 9 points: DERIV(AA) = 0 2 4 6 8 10 12 14 16 (to 1e-14). The port's
/// central difference got the two ENDS wrong (1 and 15) — the defect a user sees as
/// a bogus slope at the edges of every scan.
#[test]
fn r11_8_a_quadratic_is_exact_at_the_borders() {
    let mut i = ArrayInputs::new(9);
    i.arrays[0] = (0..9).map(|k| (k * k) as f64).collect();
    close(
        &a("DERIV(AA)", &mut i),
        &[0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0],
    );
}

/// R10-6's window, now with the fixed kernel: NDERIV fits over the WINDOW and zeroes
/// everything outside it (`aCalcPerform.c:614-616`). Compiled C, arraySize 7,
/// AA=[0,1,4,9,16,25,36]: `NDERIV(AA[1,5],1)` = [2,4,6,8,10,0,0] — the subrange shifts
/// [1,4,9,16,25] down to index 0, whose derivative as a function of index is 2(i+1).
#[test]
fn r11_8_nderiv_fits_the_window_and_zeroes_the_rest() {
    let mut i = ArrayInputs::new(7);
    i.arrays[0] = vec![0.0, 1.0, 4.0, 9.0, 16.0, 25.0, 36.0];
    close(
        &a("NDERIV(AA[1,5],1)", &mut i),
        &[2.0, 4.0, 6.0, 8.0, 10.0, 0.0, 0.0],
    );
    close(
        &a("DERIV(AA[1,5])", &mut i),
        &[2.0, 4.0, 6.0, 8.0, 10.0, 0.0, 0.0],
    );
}

/// npts = 0 gives m = 1, and `fitpoly` needs three points (`calcUtil.c:271`), so C
/// answers -1. R10-11 (`b80ef9b3`) settled that status: the engine propagates it as
/// [`CalcError::FitFailed`], and a non-zero `status` suppresses the result write
/// entirely (`aCalcPerform.c:1602-1605`), so there is no curve to compare at this
/// boundary: the engine hands back the status and no array. The zeros a compiled C
/// driver prints for `aresult` are that driver's untouched output buffer, which is a
/// record-side observation this test cannot make.
#[test]
fn r11_8_nderiv_with_no_points_per_side_has_no_fit() {
    let mut i = noisy9();
    assert_eq!(
        acalc("NDERIV(AA,0)", &mut i),
        Err(CalcError::FitFailed),
        "npts = 0 is a window of one point: C's fitpoly -1, not a silent zero curve"
    );
}
