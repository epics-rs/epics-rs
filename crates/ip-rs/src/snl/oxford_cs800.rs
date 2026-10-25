//! `Oxford_CS800.st`, compiled by snc-rs from `build.rs` and run on
//! `epics-seq-rs`. Its escaped C is `getParam`, declared by a `%%` line and
//! defined in the `%{ }%` block, and `#include`s the [`epics_seq_rs::c`]
//! glob supplies.

use epics_seq_rs::Place;
use epics_seq_rs::c::*;

macro_rules! seqg_escaped {
    (r#"#include <math.h>"#) => {};
    (r#"#include <stdio.h>"#) => {};
    (r#"#include <stdlib.h>"#) => {};
    (r#"#include <string.h>"#) => {};
    (r#"#include <epicsThread.h>"#) => {};
    (r#"int getParam(int id, int *pid, unsigned int *pval);"#) => {};
    (r#"int getParam(int id, int *param_id, unsigned int *param_value) { int i; for (i=0; i<230; i++) if (param_id[i] == id) return param_value[i]; return (-1); }"#) => {};
}

epics_seq_rs::include_program!("Oxford_CS800");

/// `getParam`: the value of the first parameter numbered `id`, else
/// `NOT_FOUND`.
fn get_param(id: i32, ids: &impl Place<[i32; 230]>, values: &impl Place<[u32; 230]>) -> i32 {
    match ids.modify(|ids| ids.iter().position(|&p| p == id)) {
        Some(i) => values.modify(|values| values[i] as i32),
        None => -1,
    }
}
use get_param as getParam;
