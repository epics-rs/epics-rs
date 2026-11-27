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

/// The C pre-scan above steps by byte value alone, so it reads the count byte
/// after `MAX`/`MIN`/`FINITE`/`ISNAN` and the bytes inside a literal as
/// opcodes (CBUG-O1, CBUG-O2). Unpatched C returns -1 for every case below
/// except the CBUG-O2 pair, which it reads out of bounds. The expected values
/// come from the compiled upstream engines with only the pre-scan changed to
/// step over whole instructions.
#[test]
fn until_prescan_steps_over_whole_instructions() {
    for (e, v) in [
        // The count byte 2 reads as `LITERAL_INT` and hides the UNTIL_END.
        ("UNTIL(A:=A+1;MAX(A,B)>3)", 1.0),
        ("UNTIL(A:=A+1;MIN(A,9)>3)", 1.0),
        ("UNTIL(A:=MIN(A,B)+1;A>3)", 0.0),
        ("UNTIL(MAX(A,B)>3)", 0.0),
        ("UNTIL(MAX(A,B)<3)", 1.0),
        // ... or the UNTIL itself.
        ("MAX(A,B)+1+UNTIL(A>-1)", 2.0),
        ("A:=-1.5;UNTIL(A:=A+1;A>3)", 1.0),
        // No loop at all: after the misread count, a byte of the double reads
        // as an unmatched UNTIL_END (sCalc for the first, aCalc the second).
        ("MAX(A,B)+1234.5678", 1234.5678),
        ("MAX(A,B)+3779.688", 3779.688),
        // CBUG-O2: a count of 1 reads as `LITERAL_DOUBLE` and walks off the
        // postfix in C.
        ("UNTIL(A:=A+1;FINITE(A))", 1.0),
        ("UNTIL(A:=A+1;MAX(A)>3)", 1.0),
    ] {
        assert_eq!(
            scalc(e, &mut StringInputs::new()),
            Ok(StackValue::Double(v)),
            "{e}"
        );
        assert_eq!(
            acalc(e, &mut ArrayInputs::new(8)),
            Ok(ArrayStackValue::Double(v)),
            "{e}"
        );
    }
    // sCalc's string literal: C's skip lands past the NUL and loses a byte,
    // here the UNTIL, so its UNTIL_END is unmatched.
    assert_eq!(
        scalc("\"ab\"+UNTIL(A:=A+1;A>3)", &mut StringInputs::new()),
        Ok(StackValue::Double(1.0))
    );
    // In the first, the lost byte is the double's opcode and one of its bytes
    // reads as UNTIL_END; the last two are corpus loops whose UNTIL_END the
    // misread count hid.
    for (e, s) in [
        ("PRINTF(\"%e\",1234.5678)", "1.234568e+03"),
        ("UNTIL(A:=MAX(A,B)+1;\"a,b\")", "a,b"),
        ("UNTIL(A:=MAX(A,B)+1;\"%d\")", "%d"),
    ] {
        match scalc(e, &mut StringInputs::new()) {
            Ok(StackValue::Str(got)) => assert_eq!(got.as_str_lossy(), s, "{e}"),
            other => panic!("{e}: {other:?}"),
        }
    }
}
