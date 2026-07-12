#![allow(clippy::approx_constant, clippy::manual_range_contains)]

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

fn eval_arr_with(expr: &str, inputs: &mut ArrayInputs) -> ArrayStackValue {
    acalc(expr, inputs).unwrap()
}

// --- Smooth ---

#[test]
fn test_smooth_basic() {
    let mut inputs = ArrayInputs::new(10);
    inputs.arrays[0] = vec![0.0, 0.0, 0.0, 0.0, 10.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let result = eval_arr_with("SMOO(AA)", &mut inputs);
    match result {
        ArrayStackValue::Array(cell) => {
            let arr = cell.buf();
            assert_eq!(arr.len(), 10);
            // Boundary points should be 0
            assert_eq!(arr[0], 0.0);
            assert_eq!(arr[1], 0.0);
            assert_eq!(arr[8], 0.0);
            assert_eq!(arr[9], 0.0);
            // Interior point 4 (spike) should be smoothed
            assert!(arr[4] < 10.0);
            assert!(arr[4] > 0.0);
        }
        _ => panic!("expected Array"),
    }
}

// --- NSmooth ---

#[test]
fn test_nsmooth() {
    let mut inputs = ArrayInputs::new(10);
    inputs.arrays[0] = vec![0.0, 0.0, 0.0, 0.0, 10.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let once = acalc("SMOO(AA)", &mut inputs).unwrap();
    let mut inputs2 = ArrayInputs::new(10);
    inputs2.arrays[0] = vec![0.0, 0.0, 0.0, 0.0, 10.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let twice = acalc("NSMOO(AA, 2)", &mut inputs2).unwrap();
    // More smoothing should reduce the peak further
    match (once, twice) {
        (ArrayStackValue::Array(a), ArrayStackValue::Array(b)) => {
            let (a, b) = (a.buf(), b.buf());
            // Peak at index 4 should be lower with more smoothing
            assert!(b[4] < a[4], "twice smoothed should be lower");
        }
        _ => panic!("expected Arrays"),
    }
}

// --- Deriv ---

#[test]
fn test_deriv_linear() {
    let mut inputs = ArrayInputs::new(10);
    // y = 2x
    inputs.arrays[0] = (0..10).map(|i| 2.0 * i as f64).collect();
    let result = eval_arr_with("DERIV(AA)", &mut inputs);
    match result {
        ArrayStackValue::Array(cell) => {
            for v in cell.buf() {
                assert!((*v - 2.0).abs() < 1e-10, "deriv={}", v);
            }
        }
        _ => panic!("expected Array"),
    }
}

// --- NDeriv ---

#[test]
fn test_nderiv_linear() {
    let mut inputs = ArrayInputs::new(10);
    inputs.arrays[0] = (0..10).map(|i| 3.0 * i as f64).collect();
    let result = eval_arr_with("NDERIV(AA, 5)", &mut inputs);
    match result {
        ArrayStackValue::Array(cell) => {
            for (i, &v) in cell.buf().iter().enumerate() {
                assert!((v - 3.0).abs() < 0.5, "nderiv[{}]={}", i, v);
            }
        }
        _ => panic!("expected Array"),
    }
}

// --- Cum ---

#[test]
fn test_cum() {
    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0, 4.0];
    let result = eval_arr_with("CUM(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![1.0, 3.0, 6.0, 10.0]));
}

// --- Cat ---

/// CAT cannot GROW the `arraySize` buffer — it writes the right operand in after
/// the left operand's window (`aCalcPerform.c:1359-1364`, `:1383-1388`). An operand
/// with no window already ends at `arraySize-1`, so there is nowhere to write and
/// CAT is a no-op. The windowed cases live in `calc_array_window.rs` (R10-6).
#[test]
fn test_cat_arrays_cannot_exceed_array_size() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    inputs.arrays[1] = vec![4.0, 5.0, 6.0];
    let result = eval_arr_with("CAT(AA, BB)", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![1.0, 2.0, 3.0]));
}

#[test]
fn test_cat_array_scalar_cannot_exceed_array_size() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    let result = eval_arr_with("CAT(AA, 4)", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![1.0, 2.0, 3.0]));
}

/// With room left in the buffer the scalar lands at `lastEl+1` and the window grows.
#[test]
fn test_cat_array_scalar_appends_when_the_buffer_has_room() {
    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0, 9.0];
    let result = eval_arr_with("CAT(AA[0,2], 4)", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![1.0, 2.0, 3.0, 4.0]));
}

// --- ArrayRandom ---

#[test]
fn test_arndm() {
    let mut inputs = ArrayInputs::new(5);
    let result = eval_arr_with("ARNDM", &mut inputs);
    match result {
        ArrayStackValue::Array(cell) => {
            let arr = cell.buf();
            assert_eq!(arr.len(), 5);
            for &v in arr {
                assert!(v >= 0.0 && v <= 1.0, "random value {} out of range", v);
            }
        }
        _ => panic!("expected Array"),
    }
}

// The FIT family lives in `calc_array_fit.rs` (R10-8) — the two tests that were
// here asserted an x/y pair of operands and a coefficient ARRAY as the result,
// neither of which aCalc has.

// --- Subrange ---

#[test]
fn test_array_subrange_not_implemented_yet() {
    // Array subrange uses [] syntax which is gated behind string feature.
    // For now, just test that the ArrayOp exists and the evaluator handles it
    // if triggered through direct compilation.
    // We'll add [] syntax for arrays when both features are enabled.
}
