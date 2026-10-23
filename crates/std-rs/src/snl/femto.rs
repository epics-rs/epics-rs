//! `femto.st`, compiled by snc-rs from `build.rs` and run on `epics-seq-rs`.
//!
//! The program's three `%%` lines are the escaped C: two `#include`s that
//! Rust has no use for, and the `powers` table its clauses index. The
//! generated code expands the fragments inside its own `mod seqg`, so the
//! table is defined here, where `use super::*` reaches it, and its arm
//! expands to nothing.

use epics_seq_rs::c::*;

macro_rules! seqg_escaped {
    (r#"#include <stdio.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"static int powers[] = {5,6,7,8,9,10,11,0,3,4,5,6,7,8,9,0};"#) => {};
}

#[allow(non_upper_case_globals)] // the C name, which the generated code uses
static powers: [i32; 16] = [5, 6, 7, 8, 9, 10, 11, 0, 3, 4, 5, 6, 7, 8, 9, 0];

epics_seq_rs::include_program!("femto");

#[cfg(test)]
mod tests {
    use super::powers;

    /// Gain is `10^powers[gainidx]`; 7 and 15 are the unused slots.
    #[test]
    fn the_gain_power_table_is_the_c_one() {
        assert_eq!(powers, [5, 6, 7, 8, 9, 10, 11, 0, 3, 4, 5, 6, 7, 8, 9, 0]);
    }
}
