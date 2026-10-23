//! `flexCombinedMotion.st`, compiled by snc-rs from `build.rs` and run on
//! `epics-seq-rs`. Its one escaped line includes `math.h` for `fabs`, which
//! the [`epics_seq_rs::c`] glob supplies.

use epics_seq_rs::c::*;

macro_rules! seqg_escaped {
    (r#"#include <math.h>"#) => {};
}

epics_seq_rs::include_program!("flexCombinedMotion");
