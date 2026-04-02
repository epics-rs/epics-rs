#![allow(clippy::approx_constant)]

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, CalcError, acalc};

fn eval_arr(expr: &str, array_size: usize) -> ArrayStackValue {
    let mut inputs = ArrayInputs::new(array_size);
    acalc(expr, &mut inputs).unwrap()
}

fn eval_arr_with(expr: &str, inputs: &mut ArrayInputs) -> ArrayStackValue {
    acalc(expr, inputs).unwrap()
}

// --- Scalar regression ---

#[test]
fn test_scalar_add() {
    assert_eq!(eval_arr("1+2", 10), ArrayStackValue::Double(3.0));
}

#[test]
fn test_scalar_sin() {
    match eval_arr("SIN(0)", 10) {
        ArrayStackValue::Double(v) => assert!(v.abs() < 1e-10),
        _ => panic!("expected Double"),
    }
}

#[test]
fn test_scalar_ternary() {
    assert_eq!(eval_arr("1?2:3", 10), ArrayStackValue::Double(2.0));
    assert_eq!(eval_arr("0?2:3", 10), ArrayStackValue::Double(3.0));
}

// --- Array variable push/fetch ---

#[test]
fn test_array_var_push() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0]; // AA
    let result = eval_arr_with("AA", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![1.0, 2.0, 3.0]));
}

#[test]
fn test_array_var_store() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    inputs.arrays[1] = vec![0.0; 3];
    acalc("BB:=AA", &mut inputs).unwrap();
    assert_eq!(inputs.arrays[1], vec![1.0, 2.0, 3.0]);
}

// --- Element-wise arithmetic ---

#[test]
fn test_array_add() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0]; // AA
    inputs.arrays[1] = vec![10.0, 20.0, 30.0]; // BB
    let result = eval_arr_with("AA+BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![11.0, 22.0, 33.0]));
}

#[test]
fn test_array_sub() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![10.0, 20.0, 30.0];
    inputs.arrays[1] = vec![1.0, 2.0, 3.0];
    let result = eval_arr_with("AA-BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![9.0, 18.0, 27.0]));
}

#[test]
fn test_array_mul() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![2.0, 3.0, 4.0];
    inputs.arrays[1] = vec![5.0, 6.0, 7.0];
    let result = eval_arr_with("AA*BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![10.0, 18.0, 28.0]));
}

#[test]
fn test_array_div() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![10.0, 20.0, 30.0];
    inputs.arrays[1] = vec![2.0, 5.0, 10.0];
    let result = eval_arr_with("AA/BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![5.0, 4.0, 3.0]));
}

// --- Broadcasting ---

#[test]
fn test_broadcast_scalar_add() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    inputs.num_vars[0] = 10.0; // A
    let result = eval_arr_with("AA+A", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![11.0, 12.0, 13.0]));
}

#[test]
fn test_broadcast_scalar_mul() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    let result = eval_arr_with("AA*2", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![2.0, 4.0, 6.0]));
}

// --- LengthMismatch ---

#[test]
fn test_length_mismatch() {
    let mut inputs = ArrayInputs::new(5);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0]; // len 3
    inputs.arrays[1] = vec![1.0, 2.0]; // len 2
    let result = acalc("AA+BB", &mut inputs);
    assert!(matches!(result, Err(CalcError::LengthMismatch)));
}

// --- Element-wise comparison ---

#[test]
fn test_array_eq() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    inputs.arrays[1] = vec![1.0, 0.0, 3.0];
    let result = eval_arr_with("AA==BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![1.0, 0.0, 1.0]));
}

// --- Element-wise logic ---

#[test]
fn test_array_and() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 0.0, 3.0];
    inputs.arrays[1] = vec![1.0, 1.0, 0.0];
    let result = eval_arr_with("AA&&BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![1.0, 0.0, 0.0]));
}

// --- Element-wise bitwise ---

#[test]
fn test_array_bitand() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![0xFF as f64, 0x0F as f64, 0xF0 as f64];
    inputs.arrays[1] = vec![0x0F as f64, 0x0F as f64, 0x0F as f64];
    let result = eval_arr_with("AA&BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![15.0, 15.0, 0.0]));
}

// --- Element-wise unary functions ---

#[test]
fn test_array_abs() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![-1.0, 2.0, -3.0];
    let result = eval_arr_with("ABS(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![1.0, 2.0, 3.0]));
}

#[test]
fn test_array_sin() {
    let mut inputs = ArrayInputs::new(2);
    inputs.arrays[0] = vec![0.0, std::f64::consts::PI / 2.0];
    let result = eval_arr_with("SIN(AA)", &mut inputs);
    match result {
        ArrayStackValue::Array(arr) => {
            assert!(arr[0].abs() < 1e-10);
            assert!((arr[1] - 1.0).abs() < 1e-10);
        }
        _ => panic!("expected Array"),
    }
}

#[test]
fn test_array_neg() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, -2.0, 3.0];
    let result = eval_arr_with("-AA", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![-1.0, 2.0, -3.0]));
}

// --- Aggregation functions ---

#[test]
fn test_avg() {
    let mut inputs = ArrayInputs::new(5);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0, 4.0, 5.0];
    let result = eval_arr_with("AVG(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(3.0));
}

#[test]
fn test_std() {
    let mut inputs = ArrayInputs::new(8);
    inputs.arrays[0] = vec![2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
    let result = eval_arr_with("STD(AA)", &mut inputs);
    match result {
        ArrayStackValue::Double(v) => assert!((v - 2.138).abs() < 0.01, "std={}", v),
        _ => panic!("expected Double"),
    }
}

#[test]
fn test_sum() {
    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0, 4.0];
    let result = eval_arr_with("SUM(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(10.0));
}

#[test]
fn test_amax() {
    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![3.0, 7.0, 2.0, 5.0];
    let result = eval_arr_with("AMAX(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(7.0));
}

#[test]
fn test_amin() {
    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![3.0, 7.0, 2.0, 5.0];
    let result = eval_arr_with("AMIN(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(2.0));
}

// --- Index functions ---

#[test]
fn test_ixmax() {
    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![3.0, 7.0, 2.0, 5.0];
    let result = eval_arr_with("IXMAX(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(1.0));
}

#[test]
fn test_ixmin() {
    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![3.0, 7.0, 2.0, 5.0];
    let result = eval_arr_with("IXMIN(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(2.0));
}

#[test]
fn test_ixz() {
    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![1.0, 2.0, 0.0, 3.0];
    let result = eval_arr_with("IXZ(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(2.0));
}

#[test]
fn test_ixz_not_found() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    let result = eval_arr_with("IXZ(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(-1.0));
}

#[test]
fn test_ixnz() {
    let mut inputs = ArrayInputs::new(4);
    inputs.arrays[0] = vec![0.0, 0.0, 5.0, 0.0];
    let result = eval_arr_with("IXNZ(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(2.0));
}

// --- FWHM ---

#[test]
fn test_fwhm_gaussian() {
    let n = 101;
    let center = 50.0;
    let sigma = 10.0;
    let data: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64;
            (-0.5 * ((x - center) / sigma).powi(2)).exp()
        })
        .collect();
    let mut inputs = ArrayInputs::new(n);
    inputs.arrays[0] = data;
    let result = eval_arr_with("FWHM(AA)", &mut inputs);
    match result {
        ArrayStackValue::Double(v) => {
            let expected = 2.3548 * sigma;
            assert!(
                (v - expected).abs() < 0.5,
                "FWHM={}, expected~{}",
                v,
                expected
            );
        }
        _ => panic!("expected Double"),
    }
}

// --- IX, ARR, ATOD ---

#[test]
fn test_ix() {
    let result = eval_arr("IX", 5);
    assert_eq!(
        result,
        ArrayStackValue::Array(vec![0.0, 1.0, 2.0, 3.0, 4.0])
    );
}

#[test]
fn test_arr() {
    let result = eval_arr("ARR(42)", 3);
    assert_eq!(result, ArrayStackValue::Array(vec![42.0, 42.0, 42.0]));
}

#[test]
fn test_atod() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![7.0, 8.0, 9.0];
    let result = eval_arr_with("ATOD(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(7.0));
}

#[test]
fn test_atod_empty() {
    let mut inputs = ArrayInputs::new(3);
    // AA is empty
    let result = eval_arr_with("ATOD(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(0.0));
}

// --- Empty array edge cases ---

#[test]
fn test_sum_empty() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![];
    // SUM of empty array (empty AA returns Double(0.0) which is TypeMismatch for SUM)
    // Actually empty AA returns Double(0.0), so SUM(0.0) is TypeMismatch
    let result = acalc("SUM(AA)", &mut inputs);
    assert!(result.is_err());
}

#[test]
fn test_avg_nonempty() {
    let mut inputs = ArrayInputs::new(1);
    inputs.arrays[0] = vec![42.0];
    let result = eval_arr_with("AVG(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(42.0));
}

// --- Complex expressions ---

#[test]
fn test_array_expression() {
    // (AA + BB) * 2
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    inputs.arrays[1] = vec![4.0, 5.0, 6.0];
    let result = eval_arr_with("(AA+BB)*2", &mut inputs);
    assert_eq!(result, ArrayStackValue::Array(vec![10.0, 14.0, 18.0]));
}

#[test]
fn test_array_numeric_vars() {
    let mut inputs = ArrayInputs::new(3);
    inputs.num_vars[0] = 10.0; // A
    inputs.num_vars[1] = 20.0; // B
    let result = eval_arr_with("A+B", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(30.0));
}
