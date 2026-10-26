//! `Keithley2kDMM.st`, compiled by snc-rs from `build.rs` and run on
//! `epics-seq-rs`. Its escaped lines are three `#include`s, whose names the
//! [`epics_seq_rs::c`] glob supplies, and the `pvname` buffer, whose arm
//! defines it inside the generated `mod seqg`.

use std::ffi::c_char;
use std::sync::Mutex;

use epics_seq_rs::Place;
use epics_seq_rs::c::*;

/// `dbDefs.h`'s, the terminating NUL included.
const PVNAME_STRINGSZ: usize = 61;

/// `%%char pvname[PVNAME_STRINGSZ];`: the buffer `sprintf` writes a units
/// PV's name into and `pvAssign` reads, one that every running copy of the
/// program shares, as C's file-scope array is.
struct PvName(Mutex<[c_char; PVNAME_STRINGSZ]>);

static PVNAME: PvName = PvName(Mutex::new([0; PVNAME_STRINGSZ]));

impl Place<[c_char; PVNAME_STRINGSZ]> for PvName {
    fn modify<R>(&self, f: impl FnOnce(&mut [c_char; PVNAME_STRINGSZ]) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }
}

impl CSrc for PvName {
    fn with_c_buffer<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        self.0.lock().unwrap().with_c_buffer(f)
    }
}

macro_rules! seqg_escaped {
    (r#"#include "stdlib.h""#) => {};
    (r#"#include "string.h""#) => {};
    (r#"#include "dbDefs.h""#) => {};
    (r#"char pvname[PVNAME_STRINGSZ];"#) => {
        static pvname: &PvName = &PVNAME;
    };
}

epics_seq_rs::include_program!("Keithley2kDMM");
