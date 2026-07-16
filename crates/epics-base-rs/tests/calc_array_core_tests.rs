#![allow(clippy::approx_constant)]

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

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
    assert_eq!(result, ArrayStackValue::array(vec![1.0, 2.0, 3.0]));
}

#[test]
fn test_array_var_store() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    inputs.arrays[1] = vec![0.0; 3];
    // R10-9: `BB:=AA` alone leaves the runtime stack empty and is
    // CALC_ERR_INCOMPLETE in aCalcPostfix — the program has to name its result.
    acalc("BB:=AA;BB", &mut inputs).unwrap();
    assert_eq!(inputs.arrays[1], vec![1.0, 2.0, 3.0]);
}

// --- Element-wise arithmetic ---

#[test]
fn test_array_add() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0]; // AA
    inputs.arrays[1] = vec![10.0, 20.0, 30.0]; // BB
    let result = eval_arr_with("AA+BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![11.0, 22.0, 33.0]));
}

#[test]
fn test_array_sub() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![10.0, 20.0, 30.0];
    inputs.arrays[1] = vec![1.0, 2.0, 3.0];
    let result = eval_arr_with("AA-BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![9.0, 18.0, 27.0]));
}

#[test]
fn test_array_mul() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![2.0, 3.0, 4.0];
    inputs.arrays[1] = vec![5.0, 6.0, 7.0];
    let result = eval_arr_with("AA*BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![10.0, 18.0, 28.0]));
}

#[test]
fn test_array_div() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![10.0, 20.0, 30.0];
    inputs.arrays[1] = vec![2.0, 5.0, 10.0];
    let result = eval_arr_with("AA/BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![5.0, 4.0, 3.0]));
}

// --- Broadcasting ---

#[test]
fn test_broadcast_scalar_add() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    inputs.num_vars[0] = 10.0; // A
    let result = eval_arr_with("AA+A", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![11.0, 12.0, 13.0]));
}

#[test]
fn test_broadcast_scalar_mul() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    let result = eval_arr_with("AA*2", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![2.0, 4.0, 6.0]));
}

// --- Operands of unequal length ---

/// R10-6: there is no such thing as a length mismatch in aCalc. Every array stack
/// cell is an `arraySize` buffer (C allocates them from a freeList of exactly that
/// size and `FETCH_ARG` copies into them), so a short input is zero-filled to
/// `arraySize` and the binary loop `for (i=0; i<arraySize; i++)` always has both
/// operands. The `CalcError::LengthMismatch` this used to expect was a port
/// invention with no C counterpart, and it is gone with the buffer invariant.
#[test]
fn test_short_inputs_are_zero_filled_to_array_size() {
    let mut inputs = ArrayInputs::new(5);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0]; // len 3
    inputs.arrays[1] = vec![1.0, 2.0]; // len 2
    let result = acalc("AA+BB", &mut inputs).expect("no length error exists in aCalc");
    assert_eq!(
        result,
        ArrayStackValue::array(vec![2.0, 4.0, 3.0, 0.0, 0.0])
    );
}

// --- Element-wise comparison ---

#[test]
fn test_array_eq() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 2.0, 3.0];
    inputs.arrays[1] = vec![1.0, 0.0, 3.0];
    let result = eval_arr_with("AA==BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![1.0, 0.0, 1.0]));
}

// --- Element-wise logic ---

#[test]
fn test_array_and() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![1.0, 0.0, 3.0];
    inputs.arrays[1] = vec![1.0, 1.0, 0.0];
    let result = eval_arr_with("AA&&BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![1.0, 0.0, 0.0]));
}

// --- Element-wise bitwise ---

#[test]
fn test_array_bitand() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![0xFF as f64, 0x0F as f64, 0xF0 as f64];
    inputs.arrays[1] = vec![0x0F as f64, 0x0F as f64, 0x0F as f64];
    let result = eval_arr_with("AA&BB", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![15.0, 15.0, 0.0]));
}

// --- Element-wise unary functions ---

#[test]
fn test_array_abs() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![-1.0, 2.0, -3.0];
    let result = eval_arr_with("ABS(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::array(vec![1.0, 2.0, 3.0]));
}

#[test]
fn test_array_sin() {
    let mut inputs = ArrayInputs::new(2);
    inputs.arrays[0] = vec![0.0, std::f64::consts::PI / 2.0];
    let result = eval_arr_with("SIN(AA)", &mut inputs);
    match result {
        ArrayStackValue::Array(cell) => {
            let arr = cell.buf();
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
    assert_eq!(result, ArrayStackValue::array(vec![-1.0, 2.0, -3.0]));
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
        ArrayStackValue::array(vec![0.0, 1.0, 2.0, 3.0, 4.0])
    );
}

#[test]
fn test_arr() {
    let result = eval_arr("ARR(42)", 3);
    assert_eq!(result, ArrayStackValue::array(vec![42.0, 42.0, 42.0]));
}

/// aCalc's array->double op is spelled `DBL` (aCalcPostfix.c:133 `{"DBL", …,
/// TO_DOUBLE}`); `ATOD`, which these two cases used to name, is in no C table.
#[test]
fn test_atod() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![7.0, 8.0, 9.0];
    let result = eval_arr_with("DBL(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(7.0));
}

#[test]
fn test_atod_empty() {
    let mut inputs = ArrayInputs::new(3);
    // AA is empty
    let result = eval_arr_with("DBL(AA)", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(0.0));
}

// --- Empty array edge cases ---

#[test]
fn test_sum_empty() {
    let mut inputs = ArrayInputs::new(3);
    inputs.arrays[0] = vec![];
    // An unset array field is pushed as Double(0.0) (see the PushDoubleVar arm),
    // and SUM of a scalar is C's pass-through scalar branch (`case ARRSUM: break;`,
    // aCalcPerform.c:1098) — so this is 0, not an error. C reaches the same 0 by a
    // different route: acalcout always allocates AA with NELM zeroed elements, so
    // SUM(AA) sums nelm zeros. Compiled aCalcPerform with AA unset: dresult=0.
    // This used to assert TypeMismatch, which is the CALC_ALARM R10-4 removed.
    assert_eq!(
        acalc("SUM(AA)", &mut inputs).unwrap(),
        ArrayStackValue::Double(0.0)
    );
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
    assert_eq!(result, ArrayStackValue::array(vec![10.0, 14.0, 18.0]));
}

#[test]
fn test_array_numeric_vars() {
    let mut inputs = ArrayInputs::new(3);
    inputs.num_vars[0] = 10.0; // A
    inputs.num_vars[1] = 20.0; // B
    let result = eval_arr_with("A+B", &mut inputs);
    assert_eq!(result, ArrayStackValue::Double(30.0));
}
