//! `busyRecord.c:175-179` is the last statement of `init_record`, and it is
//! the whole of busy's init tail:
//!
//! ```c
//! /* convert val to rval */
//! if ( prec->mask != 0 ) {
//!     if(prec->val==0) prec->rval = 0;
//!     else prec->rval = prec->mask;
//!     } else prec->rval = (epicsUInt32)prec->val;
//! ```
//!
//! Two things about it are easy to get wrong, and one case per boundary is
//! what pins each.
//!
//! The `mask == 0` arm PASSES VAL THROUGH — `rval = (epicsUInt32)prec->val` —
//! it does not zero RVAL. A busy with no hardware mask therefore drives its
//! OUT link with the value itself.
//!
//! And the conversion sits BELOW the constant-DOL load at `:151-159`, so it
//! converts whatever VAL the load left. That ordering is why it cannot live
//! in the port's `Record::init_record`, which runs before the
//! `recGblInitConstantLink` table: placed there, a `field(DOL,"5")` busy
//! reaches its first process with RVAL=0 instead of MASK. It lives in
//! `Record::init_record_tail`, called by the init-seed owner right where C's
//! line sits.
//!
//! Separately, `busyRecord.c:140-181` assigns neither `mlst` nor `lalm` where
//! `boRecord.c:172-173` seeds both — the reason busy overrides
//! `seed_deadband_tracking` with an empty body.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build(db_text: &str) -> Db {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Read a field at init, without processing the record — the window C's init
/// tail exists to fill.
fn field(db: &Db, rec: &str, name: &str) -> Option<EpicsValue> {
    db.get_record(rec).unwrap().read().record.get_field(name)
}

/// `mask != 0`, `val == 0` → `rval = 0`.
#[epics_macros_rs::epics_test]
async fn a_masked_busy_at_val_zero_starts_with_rval_zero() {
    let db = build(r#"record(busy, "B") { field(MASK, "4") }"#).await;
    assert_eq!(field(&db, "B", "VAL"), Some(EpicsValue::Enum(0)));
    assert_eq!(field(&db, "B", "RVAL"), Some(EpicsValue::ULong(0)));
}

/// `mask != 0`, `val != 0` → `rval = mask`, the WHOLE mask, not 1 and not
/// `val & mask`.
#[epics_macros_rs::epics_test]
async fn a_masked_busy_at_val_one_starts_with_the_whole_mask_in_rval() {
    let db = build(r#"record(busy, "B") { field(MASK, "4") field(VAL, "1") }"#).await;
    assert_eq!(field(&db, "B", "VAL"), Some(EpicsValue::Enum(1)));
    assert_eq!(field(&db, "B", "RVAL"), Some(EpicsValue::ULong(4)));
}

/// `mask == 0` → `rval = (epicsUInt32)val`. The arm that is NOT `rval = 0`:
/// a VAL that is neither 0 nor 1 must arrive in RVAL intact, so a
/// pass-through is distinguishable from both a zeroing and a `!!val`.
#[epics_macros_rs::epics_test]
async fn an_unmasked_busy_passes_val_through_to_rval() {
    let db = build(r#"record(busy, "B") { field(VAL, "7") }"#).await;
    assert_eq!(field(&db, "B", "MASK"), Some(EpicsValue::ULong(0)));
    assert_eq!(field(&db, "B", "VAL"), Some(EpicsValue::Enum(7)));
    assert_eq!(
        field(&db, "B", "RVAL"),
        Some(EpicsValue::ULong(7)),
        "C's else arm is rval = (epicsUInt32)val, not rval = 0"
    );
}

/// The ordering boundary. `field(DOL,"5")` reaches VAL only through
/// `recGblInitConstantLink` (`busyRecord.c:151-159`, VAL takes the constant's
/// BOOLEAN), which the port runs AFTER both `init_record` passes. The tail
/// must convert the post-load VAL, so RVAL is MASK here and not 0.
#[epics_macros_rs::epics_test]
async fn a_constant_dol_is_converted_because_the_tail_runs_after_the_load() {
    let db = build(r#"record(busy, "B") { field(MASK, "4") field(DOL, "5") }"#).await;
    assert_eq!(
        field(&db, "B", "VAL"),
        Some(EpicsValue::Enum(1)),
        "the constant loads into a temporary and VAL takes its BOOLEAN"
    );
    assert_eq!(
        field(&db, "B", "RVAL"),
        Some(EpicsValue::ULong(4)),
        "the init tail converts the VAL the constant DOL just set"
    );
}

/// The other half of the split: busy's init tail converts and stops. C never
/// assigns `mlst`/`lalm` in `init_record`, so both are 0 at iocInit even for
/// the record above whose VAL is already 1 — and the first `monitor()`
/// (`busyRecord.c:365`) therefore posts, as C's does.
#[epics_macros_rs::epics_test]
async fn busy_init_seeds_neither_mlst_nor_lalm() {
    let db = build(r#"record(busy, "B") { field(MASK, "4") field(DOL, "5") }"#).await;
    assert_eq!(field(&db, "B", "VAL"), Some(EpicsValue::Enum(1)));
    assert_eq!(
        field(&db, "B", "MLST"),
        Some(EpicsValue::Enum(0)),
        "busyRecord.c:140-181 never assigns mlst, unlike boRecord.c:172"
    );
    assert_eq!(
        field(&db, "B", "LALM"),
        Some(EpicsValue::Enum(0)),
        "nor lalm, unlike boRecord.c:173"
    );
}
