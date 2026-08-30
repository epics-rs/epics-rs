//! Every cell below C's `if (!pdset) return S_dev_noDSET` stays at its `.dbd`
//! initial for a record whose DTYP names device support nobody registered.
//!
//! ```c
//! if (!(pdset = (aidset *)(prec->dset))) {
//!     recGblRecordError(S_dev_noDSET, prec, "ai: init_record");
//!     return(S_dev_noDSET);
//! }
//! ...
//! prec->mlst = prec->val;
//! prec->alst = prec->val;
//! prec->lalm = prec->val;
//! ```
//! (`aiRecord.c:105-129`; `aoRecord.c:107-160` is the same shape around
//! `prec->init = TRUE` and `oval = pval = val`.)
//!
//! Measured on softIoc R7.0.10 against a dbd declaring `device(ai, CONSTANT,
//! devAiNoSuch, "Missing Device")` — a menu choice C accepts and a dset C
//! cannot find, which is the only way to reach a NULL dset from a `.db`.
//! C reads `ALST 0 LALM 0 MLST 0 INIT 0` on `field(VAL,"1.5")` records of 14
//! record types; the port read `1.5` in all of them, because it ran the init
//! passes at record creation and attached device support afterwards, so
//! `init_record` could not see that it had no dset.
//!
//! Boundary cases, not scenarios: dset absent / dset present (soft channel) /
//! record type that has no dset test at all.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn f64_of(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv)
        .unwrap_or_else(|e| panic!("{pv}: {e}"))
        .to_f64()
        .unwrap_or_else(|| panic!("{pv} is not numeric"))
}

/// `ai`, the type the whole family was measured on.
#[epics_macros_rs::epics_test]
async fn an_ai_with_no_dset_seeds_no_tracker() {
    let db = ioc("record(ai, \"T:A\") { field(DTYP,\"asynInt32\") field(VAL,\"1.5\") }\n").await;

    assert_eq!(f64_of(&db, "T:A.VAL"), 1.5, "the `.db` field still loaded");
    for field in ["MLST", "ALST", "LALM"] {
        assert_eq!(
            f64_of(&db, &format!("T:A.{field}")),
            0.0,
            "`aiRecord.c:127-129` is below the noDSET return; softIoc reads 0"
        );
    }
}

/// The SAME record type with device support present runs the whole tail. This
/// is the half a per-cell patch gets wrong: the seed is not deleted, it is
/// conditional on reaching it.
#[epics_macros_rs::epics_test]
async fn an_ai_with_a_soft_channel_seeds_its_trackers() {
    let db = ioc("record(ai, \"T:S\") { field(DTYP,\"Soft Channel\") field(VAL,\"1.5\") }\n").await;

    for field in ["MLST", "ALST", "LALM"] {
        assert_eq!(
            f64_of(&db, &format!("T:S.{field}")),
            1.5,
            "a dset C always links reaches `aiRecord.c:127-129`"
        );
    }
}

/// `ao`'s cell is a flag, not a tracker: `prec->init = TRUE` (`aoRecord.c:119`)
/// with `oval = pval = val` at `:155-157`, all below the same return.
#[epics_macros_rs::epics_test]
async fn an_ao_with_no_dset_is_not_marked_initialised() {
    let db = ioc("record(ao, \"T:O\") { field(DTYP,\"asynInt32\") field(VAL,\"1.5\") }\n").await;

    assert_eq!(
        db.get_pv("T:O.INIT").unwrap(),
        EpicsValue::Short(0),
        "`aoRecord.c:119` is below the noDSET return; softIoc reads INIT 0"
    );
    for field in ["OVAL", "PVAL", "MLST", "ALST", "LALM"] {
        assert_eq!(
            f64_of(&db, &format!("T:O.{field}")),
            0.0,
            "`aoRecord.c:155-160`"
        );
    }
}

/// A record type whose C `init_record` has no dset test runs its passes with
/// any DTYP at all — the gate is the refusal table, not "has a device".
#[epics_macros_rs::epics_test]
async fn a_record_type_with_no_dset_test_still_runs_its_passes() {
    let db = ioc(
        "record(calc, \"T:C\") { field(DTYP,\"asynInt32\") field(CALC,\"1\") field(VAL,\"1.5\") }\n",
    )
    .await;

    assert_eq!(
        f64_of(&db, "T:C.MLST"),
        1.5,
        "calcRecord.c has no `!pdset` branch, so nothing is skipped"
    );
}
