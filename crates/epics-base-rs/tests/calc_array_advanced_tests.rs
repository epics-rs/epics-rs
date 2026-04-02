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
        ArrayStackValue::Array(arr) => {
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
        ArrayStackValue::Array(arr) => {
            for v in &arr {
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
        ArrayStackValue::Array(arr) => {
            for (i, &v) in arr.iter().enumerate() {
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
    assert_eq!(result, ArrayStackValue::Array(vec![1.0, 3.0, 6.0, 10.0]));
}

// --- Cat ---

#[test]
fn test_cat_arrays() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    inputs.arrays[1] = vec![4.0, 5.0, 6.0];
    let result = eval_arr_with("CAT(AA, BB)", &mut inputs);
    assert_eq!(
        result,
        ArrayStackValue::Array(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
    );
}

#[test]
fn test_cat_array_scalar() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    let result = eval_arr_with("CAT(AA, 4)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![1.0, 2.0, 3.0, 4.0]));
}

// --- ArrayRandom ---

#[test]
fn test_arndm() {
    let mut inputs = ArrayInputs::new(5);
    let result = eval_arr_with("ARNDM", &mut inputs);
    match result {
        ArrayStackValue::Array(arr) => {
            assert_eq!(arr.len(), 5);
            for &v in &arr {
                assert!(v >= 0.0 && v <= 1.0, "random value {} out of range", v);
            }
        }
        _ => panic!("expected Array"),
    }
}

// --- FitPoly ---

#[test]
fn test_fitpoly_quadratic() {
    let x: Vec<f64> = (0..11).map(|i| i as f64).collect();
    let y: Vec<f64> = x.iter().map(|&xi| 1.0 + 2.0 * xi + 3.0 * xi * xi).collect();
    let mut inputs = ArrayInputs::new(11);
    inputs.arrays[0] = x; // AA = x
    inputs.arrays[1] = y; // BB = y
    let result = eval_arr_with("FITPOLY(AA, BB)", &mut inputs);
    match result {
        ArrayStackValue::Array(coeffs) => {
            assert_eq!(coeffs.len(), 3);
            assert!((coeffs[0] - 1.0).abs() < 1e-4, "a0={}", coeffs[0]);
            assert!((coeffs[1] - 2.0).abs() < 1e-4, "a1={}", coeffs[1]);
            assert!((coeffs[2] - 3.0).abs() < 1e-4, "a2={}", coeffs[2]);
        }
        _ => panic!("expected Array"),
    }
}

// --- FitQ ---

#[test]
fn test_fitq() {
    let x: Vec<f64> = (0..11).map(|i| i as f64).collect();
    let y: Vec<f64> = x.iter().map(|&xi| 1.0 + 2.0 * xi + 3.0 * xi * xi).collect();
    let mut inputs = ArrayInputs::new(11);
    inputs.arrays[0] = x;
    inputs.arrays[1] = y;
    let result = eval_arr_with("FITQ(AA, BB)", &mut inputs);
    match result {
        ArrayStackValue::Array(coeffs) => {
            assert_eq!(coeffs.len(), 4);
            assert!((coeffs[0] - 1.0).abs() < 1e-4, "a0={}", coeffs[0]);
            assert!((coeffs[3]).abs() < 1e-10, "rss={}", coeffs[3]); // perfect fit -> rss ~ 0
        }
        _ => panic!("expected Array"),
    }
}

// --- Subrange ---

#[test]
fn test_array_subrange_not_implemented_yet() {
    // Array subrange uses [] syntax which is gated behind string feature.
    // For now, just test that the ArrayOp exists and the evaluator handles it
    // if triggered through direct compilation.
    // We'll add [] syntax for arrays when both features are enabled.
}
