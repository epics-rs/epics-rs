//! R19-3 — every narrowing of a calc stack double goes through the cast owner.
//!
//! aCalc `[`/`{` (`aCalcPerform.c:1526-1534`), sCalc `[`
//! (`sCalcPerform.c:1876-1886`) and NSMOOTH's pass count (`aCalcPerform.c:581`,
//! `int j` declared at `:299`) all narrow a stack double with a bare C `(int)`.
//! Three sites open-coded that narrowing as a Rust `as` instead of asking
//! `epics_base_rs::types::c_cast`, which is the single owner of what a
//! double→int conversion means in this port.
//!
//! **There is no C value to be faithful to here.** An out-of-range double→int
//! cast is UB, and the two targets EPICS actually ships on disagree on the same
//! `dbConvert.c` / `aCalcPerform.c` source:
//!
//! ```text
//! x86-64   cvttsd2si  ->  out of range = INT_MIN,    NaN = INT_MIN
//! aarch64  fcvtzs     ->  out of range = SATURATES,  NaN = 0
//! ```
//!
//! so `AA[2,3e9]` selects NOTHING on an x86 IOC and the whole tail on a
//! Raspberry Pi / Zynq IOC. The port takes the clean value (saturate, NaN → 0 —
//! CBUG-E2), and these tests pin THAT, not either compiled C.
//!
//! So what is asserted here is the invariant, not a C transcript: a bound too
//! large to be an index selects as much as there is, a NaN bound is zero, and an
//! absurd pass count terminates. The in-range cases ARE straight C parity (both
//! targets agree inside the int range), and their values came from
//! `aCalcPerform.c` + `aCalcPostfix.c` + `calcUtil.c` compiled against base's
//! libCom on this host, arraySize 8, AA = [10,20,...,80].

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, StackValue, StringInputs, acalc, scalc};

const SZ: usize = 8;

/// AA = [10,20,...,80], arraySize 8 — the C harness's inputs.
fn inputs() -> ArrayInputs {
    let mut inp = ArrayInputs::new(SZ);
    inp.arrays[0] = (1..=SZ).map(|e| (e * 10) as f64).collect();
    inp
}

/// The full `arraySize` buffer the expression leaves behind. `[` zero-fills the
/// tail, so the buffer alone pins both the selection and the fill.
fn buf(expr: &str) -> Vec<f64> {
    eval(expr, inputs())
}

fn eval(expr: &str, mut inp: ArrayInputs) -> Vec<f64> {
    match acalc(expr, &mut inp).unwrap_or_else(|e| panic!("{expr}: {e:?}")) {
        ArrayStackValue::Array(cell) => cell.buf().to_vec(),
        other => panic!("{expr}: expected an Array result, got {other:?}"),
    }
}

/// An in-range bound cannot tell the two casts apart — the control, and the one
/// case that IS a C transcript (x86 and aarch64 agree inside the int range).
#[test]
fn an_in_range_bound_selects_the_named_elements() {
    assert_eq!(buf("AA[2,3]"), [30.0, 40.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(buf("AA[-2,-1]"), [70.0, 80.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
}

/// The boundary the cast decides. `3e9` is past `INT32_MAX` but a perfectly good
/// "to the end" bound, and the owner saturates, so the selection runs to the end
/// of the array. (An x86 C IOC answers the EMPTY array here, because `INT32_MIN`
/// wraps past the start. That is the UB divergence, not the contract.)
#[test]
fn an_end_bound_too_large_to_index_selects_to_the_end() {
    let tail = [30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 0.0, 0.0];
    assert_eq!(buf("AA[2,3e9]"), tail);
    assert_eq!(
        buf("AA[2,1e10]"),
        tail,
        "and it does not matter HOW far past"
    );
    assert_eq!(
        buf("AA[2,2147483647]"),
        tail,
        "the largest bound the int still holds — the saturated value must land \
         in the same place, or the rule has a seam at the boundary"
    );
}

/// NaN is not an index. The owner answers 0, so a NaN start bound is `AA[0,...]` —
/// which is where an x86 C IOC lands too, by the other route (`INT32_MIN`, wrapped,
/// then clamped up by `myMAX(...,0)`).
///
/// The NaN has to arrive through an input: `0/0` is NOT one. Division by zero in
/// aCalc is `myMAXFLOAT` (`aCalcPerform.c:690-695`), a huge FINITE value, which is
/// the next case down.
#[test]
fn a_nan_start_bound_is_zero() {
    let mut inp = inputs();
    inp.num_vars[0] = f64::NAN;
    assert_eq!(
        eval("AA[A,3]", inp),
        [10.0, 20.0, 30.0, 40.0, 0.0, 0.0, 0.0, 0.0]
    );
}

/// The far side of the same boundary: a START bound past the end selects nothing —
/// because there is nothing there, not because the cast wrapped. `0/0` is C's
/// `myMAXFLOAT` and so belongs here, not with the NaN.
#[test]
fn a_start_bound_past_the_end_selects_nothing() {
    assert_eq!(buf("AA[3e9,3e9]"), [0.0; SZ]);
    assert_eq!(buf("AA[0/0,3]"), [0.0; SZ], "myMAXFLOAT is past the end");
}

/// sCalc's `[` narrows its bounds with the same `(int)`
/// (`sCalcPerform.c:1876,1883`), over `strlen` instead of `arraySize`, and must
/// reach the same owner.
#[test]
fn the_string_subrange_bound_takes_the_same_cast() {
    let mut inp = StringInputs::new();
    inp.str_vars[0] = "abcdef".into();

    assert_eq!(
        scalc("AA[1,2]", &mut inp).unwrap(),
        StackValue::Str("bc".into()),
        "the in-range control"
    );
    assert_eq!(
        scalc("AA[1,3e9]", &mut inp).unwrap(),
        StackValue::Str("bcdef".into()),
        "an end bound too large to index runs to the end of the string"
    );
    inp.num_vars[0] = f64::NAN;
    assert_eq!(
        scalc("AA[A,2]", &mut inp).unwrap(),
        StackValue::Str("abc".into()),
        "a NaN start bound is zero"
    );
}

/// NSMOOTH's pass count is the same narrowing (`aCalcPerform.c:581`) — and it is
/// the one site where routing it through the owner is not enough. C's `int j`
/// holds 2e9 perfectly well, so the pass loop is unbounded even with a FLAWLESS
/// cast; under Rust's `as usize` an absurd count became `usize::MAX` and the
/// engine never returned.
///
/// What bounds it is not the count but the fixed point: `smooth` is a pure
/// function, so once a pass reproduces its own input every later pass is a no-op.
/// This asserts that stopping there is result-PRESERVING — an uncountable request
/// and a count long past convergence must land on the same array — and, by
/// returning at all, that it terminates.
#[test]
fn an_uncountable_nsmooth_pass_count_converges_instead_of_looping() {
    // NOT a ramp: a linear array is already a fixed point of the 1-4-6-4-1 kernel,
    // so it would pass this test even with the loop still unbounded.
    let mut inp = ArrayInputs::new(SZ);
    inp.arrays[0] = vec![10.0, 20.0, 30.0, 40.0, 5.0, 6.0, 7.0, 8.0];

    let converged = eval("NSMOO(AA,10000)", inp.clone());
    assert_eq!(
        eval("NSMOO(AA,1e30)", inp.clone()),
        converged,
        "a count no int can hold must give the value the passes converge to"
    );
    assert_eq!(
        eval("NSMOO(AA,2e9)", inp),
        converged,
        "and so must a count that IS representable but still unrunnable — the cast \
         was never what bounded this loop"
    );
}

/// The other end of the count: a negative pass count is zero passes. Both C targets
/// agree (the loop bound `k < j+firstEl` never holds), so this one is parity, and
/// it guards the `.max(0)` the `as usize` conversion needs.
#[test]
fn a_negative_nsmooth_pass_count_does_nothing() {
    let ramp: Vec<f64> = (1..=SZ).map(|e| (e * 10) as f64).collect();
    assert_eq!(buf("NSMOO(AA,-1)"), ramp);
    assert_eq!(
        buf("NSMOO(AA,-3e9)"),
        ramp,
        "and one that is out of range too"
    );
}
