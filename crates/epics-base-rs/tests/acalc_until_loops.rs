//! W10-A3 — aCalc runs UNTIL loops. The port had no `Opcode::Control` arm in the
//! array evaluator at all, so every aCalc expression containing an UNTIL failed
//! the perform (`Err(Internal)`), even though the compiler accepted it.
//!
//! aCalc carries its own copy of sCalc's UNTIL machinery: the element table
//! compiles `UNTIL` (`aCalcPostfix.c:200`), `aCalcPerform` pre-scans the postfix
//! for UNTIL locations (`:345-386`), and the evaluator implements both halves:
//!
//! ```c
//! case UNTIL:
//!     for (i=0; i<MAX_UNTIL_OP; i++)
//!         if (until_scratch[i].until_loc == post-1) { until_scratch[i].ps = ps; break; }
//!     break;
//!
//! case UNTIL_END:
//!     if (++loopsDone > aCalcLoopMax) break;      /* give up, no error */
//!     if (ps->d==0) {
//!         --post;
//!         for (i=0; i<MAX_UNTIL_OP; i++)
//!             if (until_scratch[i].until_end_loc == post) {
//!                 ps = until_scratch[i].ps;      /* wind the stack back */
//!                 post = until_scratch[i].until_loc;
//!                 break;
//!             }
//!         break;
//!     }
//!     break;
//! ```
//! (`aCalcPerform.c:1551-1590`.)
//!
//! NOTE: the wave-11 brief's premise for this finding — "aCalc UNTIL is a NO-OP in
//! C (aCalcPerform.c has no case; default: break)" — is refuted by both the source
//! above and the compiled binary. A no-op UNTIL would run the body ONCE and answer
//! the first (false) condition, i.e. 0. Compiled aCalc answers 1 and leaves A at 4,
//! which is a loop that ran four times. These tests pin the compiled C, not the
//! brief.
//!
//! Every expected value below is an output of the compiled upstream
//! `aCalcPostfix.c` + `aCalcPerform.c`.

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, acalc};

/// Compiled C: `A=4`, result `1` — the body ran four times (A: 1, 2, 3, 4) and
/// exited when `A>3` first held, leaving that condition on the stack as the value
/// of the `UNTIL(...)`. A no-op UNTIL would give `A=1` and result `0`.
#[test]
fn the_loop_body_repeats_until_the_condition_holds() {
    let mut inputs = ArrayInputs::new(8);
    let result = acalc("A:=0;UNTIL(A:=A+1;A>3)", &mut inputs).expect("perform must succeed");
    assert_eq!(result, ArrayStackValue::Double(1.0));
    assert_eq!(inputs.num_vars[0], 4.0);
}

/// The same loop with a further ceiling — compiled C: `A=11`, result `1`.
#[test]
fn the_iteration_count_follows_the_condition() {
    let mut inputs = ArrayInputs::new(8);
    let result = acalc("A:=0;UNTIL(A:=A+1;A>10)", &mut inputs).expect("perform must succeed");
    assert_eq!(result, ArrayStackValue::Double(1.0));
    assert_eq!(inputs.num_vars[0], 11.0);
}

/// A condition that never holds is NOT an error in C: `if (++loopsDone >
/// aCalcLoopMax) break;` simply stops looping and the perform returns 0 with the
/// last condition as the value. `aCalcLoopMax` is 1000 (`aCalcPerform.c:70`), and
/// `loopsDone` is pre-incremented, so the body runs 1001 times.
/// Compiled C: `A=1001`, result `0`, status OK.
#[test]
fn running_out_of_loops_stops_looping_without_an_error() {
    let mut inputs = ArrayInputs::new(8);
    let result = acalc("A:=0;UNTIL(A:=A+1;0)", &mut inputs).expect("perform must succeed");
    assert_eq!(result, ArrayStackValue::Double(0.0));
    assert_eq!(inputs.num_vars[0], 1001.0);
}

/// An UNTIL whose condition is true on the first pass runs its body once.
/// Compiled C: `UNTIL(1)` = 1.
#[test]
fn a_condition_that_is_true_at_once_runs_the_body_once() {
    let mut inputs = ArrayInputs::new(8);
    let result = acalc("UNTIL(1)", &mut inputs).expect("perform must succeed");
    assert_eq!(result, ArrayStackValue::Double(1.0));
}
