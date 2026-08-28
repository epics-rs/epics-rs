//! R10-6 — an aCalc array stack value carries C's ACTIVE WINDOW (`stackElement`,
//! `aCalcPerform.c:74-80`), and every reduction folds over the window, not over the
//! whole `arraySize` buffer.
//!
//! ```c
//! void calcFirstLast(stackElement *ps, int *firstEl, int *lastEl, int arraySize) {
//!     if (ps->numEl != -1) { *firstEl = ps->firstEl; *lastEl = ps->firstEl + ps->numEl - 1; }
//!     else                 { *firstEl = 0;           *lastEl = arraySize-1; }
//! }
//! ```
//!
//! SUBRANGE, SUBRANGE_IP and CAT set the window; AMAX/AMIN/IXMAX/IXMIN/IXZ/IXNZ/
//! AVERAGE/STD_DEV/FWHM/ARRSUM/SMOOTH/DERIV/NDERIV/FIT* read it. The port kept only
//! the vector, so every reduction after a subrange folded over the zero fill:
//! `AMIN(AA[1,3])` answered 0 (a tail zero) where C answers 20.
//!
//! Every expectation below is the output of a driver compiled from
//! `/home/stevek/work/epics-modules/calc/calcApp/src/{aCalcPerform,aCalcPostfix,calcUtil}.c`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

fn inputs6() -> ArrayInputs {
    // Compiled-C: arraySize 6, AA=[10,20,30,40,50,60].
    let mut i = ArrayInputs::new(6);
    i.arrays[0] = vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0];
    i
}

fn d(expr: &str, inputs: &mut ArrayInputs) -> f64 {
    match acalc(expr, inputs).expect("status 0") {
        ArrayStackValue::Double(v) => v,
        other => panic!("expected a Double result, got {other:?}"),
    }
}

fn a(expr: &str, inputs: &mut ArrayInputs) -> Vec<f64> {
    match acalc(expr, inputs).expect("status 0") {
        ArrayStackValue::Array(cell) => cell.buf().to_vec(),
        other => panic!("expected an Array result, got {other:?}"),
    }
}

/// `[i,j]` selects an inclusive range, shifts it down to index 0, zero-fills the
/// tail — and sets `numEl = 1+j-i`. The reductions then see three elements, not
/// six. Compiled C: AMIN 20, AMAX 40, AVG 30, SUM 90, IXMAX 2, IXMIN 0.
#[test]
fn r10_6_subrange_window_bounds_every_reduction() {
    let mut i = inputs6();
    assert_eq!(a("AA[1,3]", &mut i), vec![20.0, 30.0, 40.0, 0.0, 0.0, 0.0]);
    assert_eq!(
        d("AMIN(AA[1,3])", &mut i),
        20.0,
        "AMIN over the window, not the zero fill"
    );
    assert_eq!(d("AMAX(AA[1,3])", &mut i), 40.0);
    assert_eq!(
        d("AVG(AA[1,3])", &mut i),
        30.0,
        "divisor is 1+lastEl-firstEl = 3"
    );
    assert_eq!(d("SUM(AA[1,3])", &mut i), 90.0);
    assert_eq!(d("IXMAX(AA[1,3])", &mut i), 2.0);
    assert_eq!(d("IXMIN(AA[1,3])", &mut i), 0.0);
}

/// Negative control: with no subrange there is no window (C's `numEl == -1`
/// sentinel), so the same reductions cover the whole buffer. A fix that simply
/// truncated arrays would fail here.
#[test]
fn r10_6_no_window_reduces_over_the_whole_buffer() {
    let mut i = inputs6();
    assert_eq!(d("AMIN(AA)", &mut i), 10.0);
    assert_eq!(d("AMAX(AA)", &mut i), 60.0);
    assert_eq!(d("AVG(AA)", &mut i), 35.0);
    assert_eq!(d("SUM(AA)", &mut i), 210.0);
    assert_eq!(d("IXMAX(AA)", &mut i), 5.0);
}

/// `{i,j}` (SUBRANGE_IP) keeps the elements where they are, zeroes everything
/// outside — and sets `numEl = j+1`, so the window STARTS AT 0 and includes the
/// zeroed head. Compiled C: `AVG(AA{1,3})` is 22.5 = (0+20+30+40)/4, not 30.
#[test]
fn r10_6_subrange_in_place_window_includes_the_zeroed_head() {
    let mut i = inputs6();
    assert_eq!(a("AA{1,3}", &mut i), vec![0.0, 20.0, 30.0, 40.0, 0.0, 0.0]);
    assert_eq!(d("AVG(AA{1,3})", &mut i), 22.5);
    assert_eq!(d("SUM(AA{1,3})", &mut i), 90.0);
    assert_eq!(
        d("AMIN(AA{1,3})", &mut i),
        0.0,
        "the zeroed head IS in the window"
    );
}

/// C clamps the upper bound to `arraySize` (not `arraySize-1`), so `AA[1,99]` has
/// `numEl = 6` — a six-element window over a buffer whose last element is the zero
/// left by the shift. Compiled C: AVG = 200/6 = 33.333..., the tail zero included.
#[test]
fn r10_6_subrange_upper_bound_clamps_to_array_size() {
    let mut i = inputs6();
    assert_eq!(
        a("AA[1,99]", &mut i),
        vec![20.0, 30.0, 40.0, 50.0, 60.0, 0.0]
    );
    assert!((d("AVG(AA[1,99])", &mut i) - 200.0 / 6.0).abs() < 1e-12);
}

/// `numEl = 1+j-i` is a COUNT, and C stores it in the same field as the -1 "no
/// window" sentinel. So the two degenerate ranges are not the same degenerate:
///
/// * `AA[2,1]` — count 0: an empty window, and C's AVERAGE divides by it. The
///   seed `a[firstEl]` is still read (in bounds), and SUBRANGE has just zeroed the
///   whole buffer, so the answer is 0/0 = NaN.
/// * `AA[3,1]` — count -1: ALIASES the sentinel, so the window reads back as the
///   FULL buffer, whose elements SUBRANGE has also just zeroed. AVG is 0.
#[test]
fn r10_6_degenerate_windows_split_on_the_sentinel() {
    let mut i = inputs6();
    assert!(
        d("AVG(AA[2,1])", &mut i).is_nan(),
        "numEl == 0: C divides the seed by an empty window"
    );

    let mut i = inputs6();
    assert_eq!(
        d("AVG(AA[3,1])", &mut i),
        0.0,
        "numEl == -1 aliases the sentinel: the whole (zeroed) buffer averages to 0"
    );
}

/// CAT is a window writer, not a vector append. Array+scalar writes the scalar at
/// `lastEl+1` and grows the window — but only if the `arraySize` buffer has room,
/// so a full left operand makes CAT a no-op. Compiled C, arraySize 6,
/// AA=[1,2,3,4,5,6]: `CAT(AA[0,1],5)` -> [1,2,5,0,0,0] with numEl 3 (AVG 2.667),
/// and `CAT(AA,9)` -> AA unchanged.
#[test]
fn r10_6_cat_appends_inside_the_buffer_and_grows_the_window() {
    let mut i = ArrayInputs::new(6);
    i.arrays[0] = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    i.arrays[1] = vec![7.0, 8.0, 9.0, 0.0, 0.0, 0.0];

    assert_eq!(
        a("CAT(AA[0,1],5)", &mut i),
        vec![1.0, 2.0, 5.0, 0.0, 0.0, 0.0]
    );
    assert!((d("AVG(CAT(AA[0,1],5))", &mut i) - 8.0 / 3.0).abs() < 1e-12);

    assert_eq!(
        a("CAT(AA[0,1],BB[0,1])", &mut i),
        vec![1.0, 2.0, 7.0, 8.0, 0.0, 0.0]
    );

    assert_eq!(
        a("CAT(AA,9)", &mut i),
        vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
        "no window means lastEl+1 == arraySize: C's `if (lastEl+1 < arraySize)` fails"
    );
    assert_eq!(
        a("CAT(9,AA)", &mut i),
        vec![9.0, 9.0, 9.0, 9.0, 9.0, 9.0],
        "the LEFT operand is promoted (toArray), filling the buffer; nothing fits after it"
    );
}

/// R11-7 — and CAT of two SCALARS is a no-op: `case CAT: break;` (`:1400`). With
/// neither operand an array the two-arg dispatch takes the scalar branch, and the left
/// operand's cell IS the result cell, so the answer is the LEFT scalar — still a
/// scalar. Compiled C, A=5, B=7: `CAT(A,B)` is 5, and `AVG(CAT(A,B))` is 5.
///
/// The port built a two-element array [5,7]. C has nowhere to put one: CAT
/// concatenates INTO the left operand's buffer, and a scalar has no buffer.
#[test]
fn r11_7_cat_of_two_scalars_is_the_left_scalar() {
    let mut i = ArrayInputs::new(6);
    i.num_vars[0] = 5.0;
    i.num_vars[1] = 7.0;

    assert_eq!(
        acalc("CAT(A,B)", &mut i).unwrap(),
        ArrayStackValue::Double(5.0),
        "the result must stay a SCALAR, not become [5,7]"
    );
    assert_eq!(
        d("AVG(CAT(A,B))", &mut i),
        5.0,
        "a two-element array would have averaged to 6"
    );
    assert_eq!(
        d("CAT(CAT(A,B),C)", &mut i),
        5.0,
        "chaining does not accumulate: each CAT is still the left scalar"
    );
}

/// The negative control for that: as soon as EITHER operand is an array, CAT is the
/// window writer again — the scalar no-op is the scalar branch's rule alone.
#[test]
fn r11_7_cat_still_concatenates_when_an_operand_is_an_array() {
    let mut i = ArrayInputs::new(6);
    i.num_vars[1] = 7.0;
    i.arrays[0] = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];

    assert_eq!(
        a("CAT(AA[0,1],B)", &mut i),
        vec![1.0, 2.0, 7.0, 0.0, 0.0, 0.0]
    );
}
