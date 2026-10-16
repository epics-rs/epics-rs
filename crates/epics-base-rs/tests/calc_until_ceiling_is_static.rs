//! R13-7 — the UNTIL ceiling is a STATIC pre-scan of the postfix, not a count of
//! UNTILs executed.
//!
//! ```c
//! /* find all UNTIL operators in postfix, noting their locations */
//! for (i=0, post=postfix; *post != END_EXPRESSION; post++) {
//!     switch (*post) {
//!     case UNTIL:
//!         until_scratch[i].until_loc = post;
//!         i++;
//!         if (i>9) { printf("sCalcPerform: too many UNTILs\n"); return(-1); }
//!         break;
//! ```
//! (`sCalcPerform.c:341-365`; `aCalcPerform.c:355-390` is the same loop with
//! `MAX_UNTIL_OP` = 10 for the literal.)
//!
//! The loop runs to completion before a single opcode executes and counts UNTIL
//! **opcodes present**, so reachability is irrelevant. Both engines used to count at
//! run time instead — a mark was added the first time each UNTIL was reached — so
//! ten UNTILs parked on a branch the conditional never takes evaluated happily where
//! compiled C fails the whole perform.
//!
//! Both expected values below are outputs of the compiled upstream engines:
//!
//! ```text
//! ./scalc '0?(UNTIL(1)+...x10):7'  ->  sCalcPerform: too many UNTILs / PERFORM ERROR (-1)
//! ./scalc '0?(UNTIL(1)+...x9):7'   ->  d=7
//! ./acalc '0?(UNTIL(1)+...x10):7'  ->  sCalcPerform: too many UNTILs / PERFORM ERROR (-1)
//! ./acalc '0?(UNTIL(1)+...x9):7'   ->  d=7
//! ```

use epics_base_rs::calc::{ArrayInputs, ArrayStackValue, StackValue, StringInputs, acalc, scalc};

/// `0?(UNTIL(1)+UNTIL(1)+...):7` — `n` UNTILs, none of them ever reached, because
/// the condition is 0 and they all sit on the true branch.
fn dead_branch_with_untils(n: usize) -> String {
    format!("0?({}):7", vec!["UNTIL(1)"; n].join("+"))
}

/// Ten UNTIL opcodes in the postfix fail the perform even though the branch holding
/// them is never taken.
#[test]
fn ten_unreached_untils_fail_the_perform() {
    assert!(scalc(&dead_branch_with_untils(10), &mut StringInputs::new()).is_err());
    assert!(acalc(&dead_branch_with_untils(10), &mut ArrayInputs::new(8)).is_err());
}

/// Nine is under the ceiling, so the same expression runs and takes the false
/// branch: 7.
#[test]
fn nine_unreached_untils_are_fine() {
    assert_eq!(
        scalc(&dead_branch_with_untils(9), &mut StringInputs::new()),
        Ok(StackValue::Double(7.0))
    );
    assert_eq!(
        acalc(&dead_branch_with_untils(9), &mut ArrayInputs::new(8)),
        Ok(ArrayStackValue::Double(7.0))
    );
}

/// The C pre-scan above skips bytes by opcode value alone. A vararg count
/// byte of 2 reads as `LITERAL_INT`, so the scan steps over the UNTIL_END and
/// the first false condition fails the perform with -1: the port reproduces
/// that (CBUG-O1). A count of 1 reads as `LITERAL_DOUBLE` and walks off the
/// postfix (ASan heap-buffer-overflow at `sCalcPerform.c:340` /
/// `aCalcPerform.c:356` for `UNTIL(A:=A+1;FINITE(A))`); there the port
/// decodes whole opcodes instead (CBUG-O2).
#[test]
fn until_prescan_reads_vararg_counts_as_c_does() {
    for e in [
        "UNTIL(A:=A+1;MAX(A,B)>3)",
        "UNTIL(A:=A+1;MIN(A,9)>3)",
        "UNTIL(MAX(A,B)>3)",
    ] {
        assert!(scalc(e, &mut StringInputs::new()).is_err(), "{e}");
        assert!(acalc(e, &mut ArrayInputs::new(8)).is_err(), "{e}");
    }
    // A condition true on the first pass never asks for its UNTIL, in C too.
    assert_eq!(
        scalc("UNTIL(MAX(A,B)<3)", &mut StringInputs::new()),
        Ok(StackValue::Double(1.0))
    );
    // The literal skips are C's sizes: an 8-byte double whose bytes hold no
    // UNTIL_END. C returns 1.
    // A missed UNTIL is C's -1 when the loop asks for it: the count byte 2
    // hides the UNTIL after `MAX(A,B)` from the scan. C returns -1.
    for (e, c) in [
        ("A:=-1.5;UNTIL(A:=A+1;A>3)", Some(1.0)),
        ("MAX(A,B)+1+UNTIL(A>-1)", None),
    ] {
        let s = scalc(e, &mut StringInputs::new());
        let a = acalc(e, &mut ArrayInputs::new(8));
        match c {
            Some(v) => {
                assert_eq!(s, Ok(StackValue::Double(v)), "{e}");
                assert_eq!(a, Ok(ArrayStackValue::Double(v)), "{e}");
            }
            None => {
                assert!(s.is_err(), "{e}: {s:?}");
                assert!(a.is_err(), "{e}: {a:?}");
            }
        }
    }
    // Past the end, where C reads out of bounds, the loops run (CBUG-O2).
    // `MAX(A)` starts false, so the loop needs its UNTIL found.
    for e in ["UNTIL(A:=A+1;FINITE(A))", "UNTIL(A:=A+1;MAX(A)>3)"] {
        assert_eq!(
            scalc(e, &mut StringInputs::new()),
            Ok(StackValue::Double(1.0)),
            "{e}"
        );
        assert_eq!(
            acalc(e, &mut ArrayInputs::new(8)),
            Ok(ArrayStackValue::Double(1.0)),
            "{e}"
        );
    }
}
