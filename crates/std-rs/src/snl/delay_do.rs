//! `delayDo.st`, compiled by snc-rs from `build.rs` and run on `epics-seq-rs`.
//!
//! The program has no escaped C. `DEBUG_PRINT` in `seqPVmacros.h` expands to
//! `printf` and `epicsThreadSleep`, which the generated code leaves
//! unqualified and so resolve here, through the `c` glob.

use epics_seq_rs::c::*;

epics_seq_rs::include_program!("delayDo");
