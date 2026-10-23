//! `sncqxbpm.st`, compiled by snc-rs from `build.rs` and run on `epics-seq-rs`.
//!
//! The only escaped C is three `#include`s; every C call the program makes
//! is to the C library [`epics_seq_rs::c`] provides.

use epics_seq_rs::c::*;

macro_rules! seqg_escaped {
    (r#"#include <math.h>"#) => {};
    (r#"#include <string.h>"#) => {};
    (r#"#include <epicsThread.h>"#) => {};
}

epics_seq_rs::include_program!("sncqxbpm");
