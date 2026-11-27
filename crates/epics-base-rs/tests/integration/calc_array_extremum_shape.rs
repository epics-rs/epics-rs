//! aCalc's four extremum operators — `>?`, `<?`, `MAX()`, `MIN()` — and the
//! OPERAND SHAPE each answers with.
//!
//! All four sit inside C's array dispatch: `>?`/`<?` are ordinary members of the
//! two-arg switch (`aCalcPerform.c:1326-1327`, applied at `:1351-1352` array/array,
//! `:1365-1366` array/scalar, `:1392-1393` scalar/scalar), and `MAX()`/`MIN()` have
//! their own vararg arm (`:1144-1180`) that branches on whether ANY argument is an
//! array. So an array operand ANYWHERE yields an element-wise ARRAY result — never
//! the scalar the port used to build from each operand's a[0].
//!
//! Every expected value below was read off the compiled upstream aCalc
//! (`aCalcPerform.c` + `aCalcPostfix.c` + `calcUtil.c`, gcc 13, arraySize 6).

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

/// AA=[1,5,2,8,3,9], BB=[4,4,4,4,4,4], CC=[0,0,7,0,0,0]
fn inputs() -> ArrayInputs {
    let mut i = ArrayInputs::new(6);
    i.arrays[0] = vec![1.0, 5.0, 2.0, 8.0, 3.0, 9.0];
    i.arrays[1] = vec![4.0; 6];
    i.arrays[2] = vec![0.0, 0.0, 7.0, 0.0, 0.0, 0.0];
    i
}

fn arr(expr: &str) -> Vec<f64> {
    let mut i = inputs();
    match acalc(expr, &mut i).unwrap() {
        ArrayStackValue::Array(c) => c.buf().to_vec(),
        ArrayStackValue::Double(d) => panic!("expected an ARRAY result, got the scalar {d}"),
    }
}

fn d(expr: &str) -> f64 {
    let mut i = inputs();
    match acalc(expr, &mut i).unwrap() {
        ArrayStackValue::Double(v) => v,
        ArrayStackValue::Array(c) => {
            panic!("expected a SCALAR result, got the array {:?}", c.buf())
        }
    }
}

// --- `>?` / `<?`: every shape with an array operand answers an array ---

#[test]
fn maxval_array_array_is_elementwise() {
    // compiled C: `AA>?BB` -> [4,5,4,8,4,9]
    assert_eq!(arr("AA>?BB"), vec![4.0, 5.0, 4.0, 8.0, 4.0, 9.0]);
}

#[test]
fn maxval_array_scalar_is_elementwise() {
    // compiled C: `AA>?4` -> [4,5,4,8,4,9]
    assert_eq!(arr("AA>?4"), vec![4.0, 5.0, 4.0, 8.0, 4.0, 9.0]);
}

#[test]
fn maxval_scalar_array_is_elementwise() {
    // compiled C: `4>?AA` -> [4,5,4,8,4,9]. C promotes the LEFT operand with
    // `toArray(ps,1)` (`:1338`) and the result is that cell, so the mixed shapes
    // are symmetric here.
    assert_eq!(arr("4>?AA"), vec![4.0, 5.0, 4.0, 8.0, 4.0, 9.0]);
}

#[test]
fn minval_array_array_is_elementwise() {
    // compiled C: `AA<?BB` -> [1,4,2,4,3,4]
    assert_eq!(arr("AA<?BB"), vec![1.0, 4.0, 2.0, 4.0, 3.0, 4.0]);
}

#[test]
fn maxval_scalar_scalar_stays_scalar() {
    // compiled C: `3>?7` -> VAL 7, and the result really is a scalar (AVAL is the
    // broadcast 7). No array operand, no array result.
    assert_eq!(d("3>?7"), 7.0);
    assert_eq!(d("7<?3"), 3.0);
}

// --- vararg MAX()/MIN() ---

#[test]
fn vararg_max_with_array_arg_is_elementwise() {
    // compiled C: `MAX(AA,BB)` -> [4,5,4,8,4,9]; `MAX(AA,4)` and `MAX(4,AA)` alike.
    assert_eq!(arr("MAX(AA,BB)"), vec![4.0, 5.0, 4.0, 8.0, 4.0, 9.0]);
    assert_eq!(arr("MAX(AA,4)"), vec![4.0, 5.0, 4.0, 8.0, 4.0, 9.0]);
    assert_eq!(arr("MAX(4,AA)"), vec![4.0, 5.0, 4.0, 8.0, 4.0, 9.0]);
}

#[test]
fn vararg_min_with_array_arg_is_elementwise() {
    // compiled C: `MIN(AA,BB,2)` -> [1,2,2,2,2,2]
    assert_eq!(arr("MIN(AA,BB,2)"), vec![1.0, 2.0, 2.0, 2.0, 2.0, 2.0]);
}

#[test]
fn vararg_max_folds_every_argument() {
    // compiled C: `MAX(AA,BB,CC,1)` -> [4,5,7,8,4,9]
    assert_eq!(arr("MAX(AA,BB,CC,1)"), vec![4.0, 5.0, 7.0, 8.0, 4.0, 9.0]);
}

#[test]
fn vararg_max_all_scalars_stays_scalar() {
    // compiled C: `MAX(3,7,5)` -> 7
    assert_eq!(d("MAX(3,7,5)"), 7.0);
    assert_eq!(d("MIN(3,7,5)"), 3.0);
}

// --- the window: which argument's cell is the result cell ---

#[test]
fn vararg_extremum_result_cell_is_the_first_argument() {
    // C coerces the BOTTOMMOST argument (`ps1 = ps - (nargs-1)`, `:1161-1162`) and
    // folds into it, so the result carries the FIRST argument's window.
    //
    // compiled C: AVG(MAX(AA[1,3],0)) is 5   — the 3-element window [5,2,8] survives
    //             AVG(MAX(0,AA[1,3])) is 2.5 — the promoted scalar's window is the
    //                                          whole buffer, so the zero tail counts
    assert_eq!(d("AVG(MAX(AA[1,3],0))"), 5.0);
    assert_eq!(d("AVG(MAX(0,AA[1,3]))"), 2.5);
}

#[test]
fn maxval_keeps_the_left_operands_window() {
    // compiled C: `(AA[1,3])>?0` -> [5,2,8,0,0,0] and AVG of it is 5 — the two-arg
    // result IS the left cell, so its 3-element window is untouched.
    assert_eq!(arr("(AA[1,3])>?0"), vec![5.0, 2.0, 8.0, 0.0, 0.0, 0.0]);
    assert_eq!(d("AVG((AA[1,3])>?0)"), 5.0);
}

// --- NaN: C's comparisons are bare, so which operand holds the NaN decides ---

#[test]
fn maxval_scalar_nan_follows_cs_bare_comparison() {
    // C `case MAX_VAL: if (ps->d < ps1->d) ps->d = ps1->d;` — every comparison
    // against NaN is false, so the LEFT operand simply stands.
    //   compiled C: `5>?NaN` is 5 ;  `NaN>?5` is NaN (status -1)
    assert_eq!(d("5>?ACOS(2)"), 5.0);
    assert!(d("ACOS(2)>?5").is_nan());
}

#[test]
fn vararg_extremum_scalar_nan_propagates() {
    // C's SCALAR fold alone carries `|| isnan(d)` (`:1185`, `:1187`), and it makes a
    // NaN survive from either position. compiled C: `MAX(5,NaN)`, `MAX(NaN,5)`,
    // `MIN(5,NaN)` and `MIN(NaN,5)` are all NaN.
    //
    // The port's test was inverted (it dropped a NaN accumulator), so all four
    // answered 5.
    assert!(d("MAX(5,ACOS(2))").is_nan());
    assert!(d("MAX(ACOS(2),5)").is_nan());
    assert!(d("MIN(5,ACOS(2))").is_nan());
    assert!(d("MIN(ACOS(2),5)").is_nan());
}

#[test]
fn vararg_extremum_array_branch_has_no_nan_test() {
    // The ARRAY branch is a bare `if (other > acc)`, so a NaN argument never wins.
    // compiled C: `MAX(AA,NaN)` -> AA unchanged, [1,5,2,8,3,9].
    assert_eq!(arr("MAX(AA,ACOS(2))"), vec![1.0, 5.0, 2.0, 8.0, 3.0, 9.0]);
    // And a NaN FIRST argument is promoted by `toArray(ps1,1)`, whose NaN->0 fill
    // (`aCalcPerform.c:135-138`) makes it a zero buffer before the fold.
    // compiled C: `MAX(NaN,AA)` -> [1,5,2,8,3,9]; `AA>?NaN` -> [1,5,2,8,3,9].
    assert_eq!(arr("MAX(ACOS(2),AA)"), vec![1.0, 5.0, 2.0, 8.0, 3.0, 9.0]);
    assert_eq!(arr("AA>?ACOS(2)"), vec![1.0, 5.0, 2.0, 8.0, 3.0, 9.0]);
}
