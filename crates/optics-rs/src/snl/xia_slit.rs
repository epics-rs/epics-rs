//! `xia_slit.st`, compiled by snc-rs from `build.rs` and run on
//! `epics-seq-rs`. Its escaped lines are four `#include`s; the
//! [`epics_seq_rs::c`] glob supplies what they declared.

use epics_seq_rs::c::*;

macro_rules! seqg_escaped {
    (r#"#include <stdio.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"#include <string.h>"#) => {};
    (r#"#include <epicsThread.h>"#) => {};
}

epics_seq_rs::include_program!("xia_slit");
