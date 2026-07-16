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
