//! R19-3 — a subrange bound is C's `(int)` cast, not Rust's saturating `as`.
//!
//! aCalc `[`/`{` (`aCalcPerform.c:1526-1534`) and sCalc `[`
//! (`sCalcPerform.c:1876-1886`) both narrow their numeric bounds with a bare
//! `(int)`. On x86-64 that is `cvttsd2si`: anything outside the 32-bit range —
//! and NaN, and the infinities — becomes `INT32_MIN`, which the `if (i < 0) i +=
//! k` wrap then leaves far negative, so an out-of-range END bound selects
//! NOTHING. Rust's `as i64` SATURATES instead, which clamps to the container
//! length and selects EVERYTHING from the start bound on — the opposite answer.
//!
//! Every expected value below is the output of the compiled C on this host
//! (`aCalcPerform.c` + `aCalcPostfix.c` + `calcUtil.c` + `myFreeListLib.c` linked
//! against base's libCom, arraySize 8, AA = [10,20,...,80]):
//!
//! ```text
//! AA[2,3]     arr=[30,40,0,0,0,0,0,0]
//! AA[2,3e9]   arr=[0,0,0,0,0,0,0,0]
//! AA[2,1e10]  arr=[0,0,0,0,0,0,0,0]
//! AA[-2,-1]   arr=[70,80,0,0,0,0,0,0]
//! AA[0/0,3]   arr=[10,20,30,40,0,0,0,0]
//! ```

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, StackValue, StringInputs, acalc, scalc};

const SZ: usize = 8;

/// AA = [10,20,...,80], arraySize 8 — the C harness's inputs.
fn inputs() -> ArrayInputs {
    let mut inp = ArrayInputs::new(SZ);
    inp.arrays[0] = (1..=SZ).map(|e| (e * 10) as f64).collect();
    inp
}

/// The full `arraySize` buffer C leaves behind. `[` zero-fills the tail, so the
/// buffer alone pins both the selection and the fill.
fn buf(expr: &str) -> Vec<f64> {
    let mut inp = inputs();
    match acalc(expr, &mut inp).unwrap_or_else(|e| panic!("{expr}: {e:?}")) {
        ArrayStackValue::Array(cell) => cell.buf().to_vec(),
        other => panic!("{expr}: expected an Array result, got {other:?}"),
    }
}

/// An in-range bound is unaffected by the cast — the control.
#[test]
fn an_in_range_bound_selects_the_named_elements() {
    assert_eq!(buf("AA[2,3]"), [30.0, 40.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    assert_eq!(buf("AA[-2,-1]"), [70.0, 80.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
}

/// The boundary the cast decides: 3e9 is past `INT32_MAX` but inside `i64`.
/// C's `(int)` yields `INT32_MIN`, so the range inverts and NOTHING is selected.
/// Rust's saturating `as i64` selected `AA[2..8]`.
#[test]
fn an_end_bound_past_int32_max_selects_nothing() {
    assert_eq!(buf("AA[2,3e9]"), [0.0; SZ]);
    assert_eq!(buf("AA[2,1e10]"), [0.0; SZ]);
}

/// The same cast on a NaN bound: `cvttsd2si` answers `INT32_MIN` for NaN too, and
/// the wrap + `myMAX(...,0)` clamp on `i` turns that into 0 — so `AA[0/0,3]` is
/// `AA[0,3]`, neither an error nor an empty range.
#[test]
fn a_nan_start_bound_clamps_to_zero() {
    assert_eq!(
        buf("AA[0/0,3]"),
        [10.0, 20.0, 30.0, 40.0, 0.0, 0.0, 0.0, 0.0]
    );
}

/// `INT32_MAX` itself still fits the cast — the last value that does. It clamps
/// to `arraySize` like any oversized-but-representable bound, so the tail IS
/// selected. This case separates "the cast is faithful" from "the port simply
/// refuses large bounds".
#[test]
fn an_end_bound_at_int32_max_still_selects_the_tail() {
    assert_eq!(
        buf("AA[6,2147483647]"),
        [70.0, 80.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    );
}

/// sCalc's `[` narrows its bounds with the same `(int)`
/// (`sCalcPerform.c:1876,1883`), over `strlen` instead of `arraySize`.
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
        StackValue::Str("".into()),
        "INT32_MIN end bound -> inverted range -> empty"
    );
}

/// NSMOOTH's pass count is the same `(int)` conversion (`aCalcPerform.c:581`,
/// `int j; j = ps->d;`), and its loop then runs `max(j,0)` times. Under Rust's
/// saturating `as usize` this asked for `usize::MAX` passes over the buffer —
/// the engine did not return. It must be a no-op, as it is in C.
#[test]
fn an_out_of_range_nsmooth_pass_count_does_nothing() {
    let ramp: Vec<f64> = (1..=SZ).map(|e| (e * 10) as f64).collect();
    assert_eq!(
        buf("NSMOO(AA,1e30)"),
        ramp,
        "INT32_MIN passes -> zero passes"
    );
    assert_eq!(buf("NSMOO(AA,-1)"), ramp, "a negative count -> zero passes");
}
