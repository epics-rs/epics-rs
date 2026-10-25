//! sequencer R2-2-9 `test/validate`, compiled by snc-rs from `build.rs` and
//! run on epics-seq-rs: a companion module per program, the
//! `testSupport.c` it includes ([`support`]), and the `epicsUnitTest.c`
//! calls it makes ([`unit_test`]). `tests/validate.rs` runs each program as
//! the validate `.t` scripts do and holds its results to C's.

pub mod seq_snc;
pub mod stdlib;
pub mod support;
pub mod unit_test;

/// A companion: the escaped C every program has, the include of
/// `testSupport.h`, and each other fragment's text with what it expands
/// to; then `$item`s the program names beside the escaped C.
macro_rules! program {
    ($module:ident, $stem:literal $(, $text:tt => $body:tt)* $(; $($item:item)*)?) => {
        pub mod $module {
            use crate::support::*;
            use crate::unit_test::*;

            $($($item)*)?

            macro_rules! seqg_escaped {
                (r#"#include "../testSupport.h""#) => {};
                $(($text) => $body;)*
            }

            epics_seq_rs::include_program!($stem);
        }
    };
}

program!(assign, "assign");
program!(bittypes, "bittypes");
program!(change, "change",
    r#"#include <time.h>"# => {},
    r#"#include <stdlib.h>"# => {};
    use crate::stdlib::rand;
);
program!(comma_operator, "commaOperator");
program!(entry_opte, "entryOpte");
program!(evflag, "evflag"; use epics_seq_rs::c::epicsThreadSleep;);
program!(evflag_ext, "evflagExt",
    r#"static void extfun(SS_ID ssid);"# => {},
    r#"static void extfun(SS_ID ssid) { seq_efSet(ssid, e); }"# => {
        fn extfun(ssid: &seqg_rt::StateSet<seqg_vars>) {
            ssid.ef_set(e);
        }
    }
);
program!(exit_optx, "exitOptx");
program!(local, "local");
program!(monitor_evflag, "monitorEvflag", r#"#include <stdio.h>"# => {});
program!(norace, "norace"; use crate::seq_snc::MAX_STRING_SIZE;);
program!(pv_assign_stress, "pvAssignStress");
program!(pv_assign_subst, "pvAssignSubst"; use epics_seq_rs::c::strcmp;);
program!(pv_get_async, "pvGetAsync");
program!(pv_get_complete, "pvGetComplete");
program!(pv_get_sync, "pvGetSync");
program!(pv_put_and_monitor, "pvPutAndMonitor");
program!(pv_put_async, "pvPutAsync");
program!(pv_sync_db, "pvSyncDb");
program!(pv_sync_no_db, "pvSyncNoDb");
program!(reassign, "reassign"; use crate::seq_snc::{epicsThreadGetIdSelf, seqShow};);
program!(safe_mode_not_assigned, "safeModeNotAssigned");
program!(safe_monitor, "safeMonitor"; use epics_seq_rs::c::epicsThreadSleep;);
program!(stop, "stop", r#"#include <stdlib.h>"# => {};
    use crate::seq_snc::{epicsThreadGetIdSelf, seqStop};
);
program!(userfunc, "userfunc",
    r#"static void incr(SS_ID ssId, int *pv, VAR_ID v); static void incr_compat(SS_ID ssId, int *pv, VAR_ID v); static void set_i(USER_VAR *const pVar, int value); static void null(void);"# => {},
    r#"static void incr_compat(SS_ID ssId, int *pv, VAR_ID v) { seq_pvGet(ssId, v, SYNC); *pv += 1; seq_pvPut(ssId, v, SYNC); } static void incr(SS_ID ssId, int *pv, VAR_ID v) { seq_pvGetTmo(ssId, v, SYNC, 1.0); *pv += 1; seq_pvPutTmo(ssId, v, SYNC, 1.0); } static void set_i(USER_VAR *const pVar, int value) { pVar->i = value; } static void null(void) { }"# => {
        fn incr_compat(
            ss_id: &seqg_rt::StateSet<seqg_vars>,
            pv: &seqg_rt::SeqVar<std::ffi::c_int>,
            v: u32,
        ) {
            ss_id.pv_get(seqg_rt::ChId(v), seqg_rt::CompType::Sync);
            pv.set(pv.get() + 1);
            ss_id.pv_put(seqg_rt::ChId(v), seqg_rt::CompType::Sync);
        }

        fn incr(
            ss_id: &seqg_rt::StateSet<seqg_vars>,
            pv: &seqg_rt::SeqVar<std::ffi::c_int>,
            v: u32,
        ) {
            ss_id.pv_get_tmo(seqg_rt::ChId(v), seqg_rt::CompType::Sync, 1.0);
            pv.set(pv.get() + 1);
            ss_id.pv_put_tmo(seqg_rt::ChId(v), seqg_rt::CompType::Sync, 1.0);
        }

        fn set_i(p_var: &seqg_vars, value: std::ffi::c_int) {
            p_var.i.set(value);
        }

        fn null() {}
    }
);
program!(userfunc_ef, "userfuncEf",
    r#"static void efSet_x(SS_ID ssId, EV_ID x) { seq_efSet(ssId, x); }"# => {
        // C's parameter `x` shadows the program's evflag `x`; a Rust
        // parameter cannot shadow a const.
        fn efSet_x(ss_id: &seqg_rt::StateSet<seqg_vars>, ev: u32) {
            ss_id.ef_set(seqg_rt::EfId(ev));
        }
    }
);
