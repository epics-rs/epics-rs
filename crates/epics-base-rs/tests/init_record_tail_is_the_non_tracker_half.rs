//! C's `init_record` tail does two different things, and the port has two
//! hooks for them. Each hook must do only its own half.
//!
//! ```c
//! /* boRecord.c:165-175 (R7.0.10) */
//! prec->mlst = prec->val;
//! /* convert val to rval */
//! if ( prec->mask != 0 ) {
//!     if(prec->val==0) prec->rval = 0;
//!     else prec->rval = prec->mask;
//! } else prec->rval = (epicsUInt32)prec->val;
//!
//! prec->mlst = prec->val;      /* the trackers */
//! prec->lalm = prec->val;
//! prec->oraw = prec->rval;     /* the derived output state */
//! prec->orbv = prec->rbv;
//! ```
//!
//! bo, mbbo, mbboDirect and ao carried BOTH halves inside
//! `seed_deadband_tracking`, so "seed the deadband trackers" and "convert VAL
//! to RVAL" were the same call. That is the dual meaning `busy` already had to
//! be an exception to — `busyRecord.c:176-179` converts and seeds no tracker,
//! which is why `init_record_tail` exists at all, and with the two fused the
//! hook had exactly one implementor.
//!
//! The cases below pin the split from both sides: each hook in isolation does
//! its own half and not the other's, and every build path still runs the pair.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(busy, "B")   { field(VAL, "1") field(MASK, "255") }
record(bo,   "BO")  { field(VAL, "1") }
record(ao,   "AO")  { field(VAL, "2.5") }
record(mbbo, "MO")  { field(VAL, "2") field(TWVL, "77") }
record(mbboDirect, "MD") { field(VAL, "5") }
"#;

type Db = Arc<PvDatabase>;

async fn build() -> Db {
    // `busy` is the busy module, not Base: an application that loads it says
    // so, the way a real one loads `busySupport.dbd`.
    IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn f(db: &Db, rec: &str, field: &str) -> f64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field(field)
        .and_then(|v| v.to_f64())
        .unwrap_or_else(|| panic!("{rec}.{field}"))
}

fn cell(rec: &dyn epics_base_rs::server::record::Record, field: &str) -> f64 {
    rec.get_field(field)
        .and_then(|v| v.to_f64())
        .unwrap_or_else(|| panic!("{field}"))
}

/// The tracker hook must not convert. Before the split this assertion could
/// not hold on any of the four: `seed_deadband_tracking` ran `val_to_rval` /
/// `convert` / `val_to_bits` / the OVAL store as its first statement.
#[test]
fn seed_deadband_tracking_seeds_trackers_and_derives_nothing() {
    let mut bo = create_record("bo").unwrap();
    bo.put_field("MASK", EpicsValue::ULong(255)).unwrap();
    bo.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    bo.seed_deadband_tracking();
    assert_eq!(cell(&*bo, "MLST"), 1.0, "boRecord.c:172");
    assert_eq!(cell(&*bo, "LALM"), 1.0, "boRecord.c:173");
    assert_eq!(
        cell(&*bo, "RVAL"),
        0.0,
        "the convert is boRecord.c:167-170, not the tracker lines"
    );

    let mut ao = create_record("ao").unwrap();
    ao.put_field("VAL", EpicsValue::Double(2.5)).unwrap();
    ao.seed_deadband_tracking();
    assert_eq!(cell(&*ao, "ALST"), 2.5, "aoRecord.c:158");
    assert_eq!(cell(&*ao, "OVAL"), 0.0, "OVAL is aoRecord.c:156");

    let mut md = create_record("mbboDirect").unwrap();
    md.put_field("VAL", EpicsValue::Long(5)).unwrap();
    md.put_field("RVAL", EpicsValue::ULong(5)).unwrap();
    md.seed_deadband_tracking();
    assert_eq!(cell(&*md, "MLST"), 5.0, "mbboDirectRecord.c:160");
    // Not B0: the VAL put re-derives the bit cells itself, so only ORAW
    // isolates the hook. `oraw = rval` is mbboDirectRecord.c:161.
    assert_eq!(cell(&*md, "ORAW"), 0.0, "mbboDirectRecord.c:161");
}

/// And the derived hook must not seed trackers — the direction that keeps
/// `busy` from being an exception (`busyRecord.c` assigns no `mlst`/`lalm` at
/// all, so a fused hook would have had to seed them for bo and skip them for
/// busy from the same call).
#[test]
fn init_record_tail_derives_and_seeds_no_tracker() {
    let mut bo = create_record("bo").unwrap();
    bo.put_field("MASK", EpicsValue::ULong(255)).unwrap();
    bo.put_field("VAL", EpicsValue::Enum(1)).unwrap();
    bo.init_record_tail();
    assert_eq!(cell(&*bo, "RVAL"), 255.0, "boRecord.c:167-169");
    assert_eq!(cell(&*bo, "ORAW"), 255.0, "boRecord.c:174");
    assert_eq!(cell(&*bo, "MLST"), 0.0, "boRecord.c:172 is the other hook");

    let mut mo = create_record("mbbo").unwrap();
    mo.put_field("TWVL", EpicsValue::ULong(77)).unwrap();
    mo.put_field("VAL", EpicsValue::UShort(2)).unwrap();
    mo.init_record_tail();
    assert_eq!(cell(&*mo, "RVAL"), 77.0, "mbboRecord.c:177 convert()");
    assert_eq!(
        cell(&*mo, "LALM"),
        0.0,
        "mbboRecord.c:180 is the other hook"
    );
}

/// The `.db` build path, end to end: the seed owner runs both halves, so the
/// move changed nothing any client can see.
#[epics_macros_rs::epics_test]
async fn the_db_path_runs_both_halves_of_the_tail() {
    let db = build().await;

    // busy — the record with only the derived half (`busyRecord.c:176-179`).
    assert_eq!(f(&db, "B", "RVAL"), 255.0);

    // bo — `boRecord.c:167-175`.
    assert_eq!(f(&db, "BO", "RVAL"), 1.0);
    assert_eq!(f(&db, "BO", "ORAW"), 1.0);
    assert_eq!(f(&db, "BO", "MLST"), 1.0);

    // ao — `aoRecord.c:156-161`.
    assert_eq!(f(&db, "AO", "OVAL"), 2.5);
    assert_eq!(f(&db, "AO", "PVAL"), 2.5);
    assert_eq!(f(&db, "AO", "ALST"), 2.5);

    // mbbo — `convert()` maps the state index through the value table, so RVAL
    // is the state's raw value and not the index.
    assert_eq!(f(&db, "MO", "VAL"), 2.0);
    assert_eq!(f(&db, "MO", "RVAL"), 77.0);
    assert_eq!(f(&db, "MO", "MLST"), 2.0);

    // mbboDirect — `bitsFromVAL` derives the bit cells from VAL; 5 == 0b101.
    assert_eq!(f(&db, "MD", "B0"), 1.0);
    assert_eq!(f(&db, "MD", "B1"), 0.0);
    assert_eq!(f(&db, "MD", "B2"), 1.0);
    assert_eq!(f(&db, "MD", "MLST"), 5.0);
}

/// The inline `IocBuilder::record` path, which now runs neither half itself —
/// `PvDatabase::add_record` ends in the same seed owner.
#[epics_macros_rs::epics_test]
async fn the_inline_path_runs_both_halves_of_the_tail() {
    let db = IocBuilder::new()
        .record("B2", {
            let mut r = BusyRecord::new();
            let _ = epics_base_rs::server::record::Record::put_field(
                &mut r,
                "MASK",
                EpicsValue::ULong(255),
            );
            let _ = epics_base_rs::server::record::Record::put_field(
                &mut r,
                "VAL",
                EpicsValue::Enum(1),
            );
            r
        })
        .record("BO2", {
            let mut r = epics_base_rs::server::records::bo::BoRecord::new(0);
            let _ = epics_base_rs::server::record::Record::put_field(
                &mut r,
                "VAL",
                EpicsValue::Enum(1),
            );
            r
        })
        .build()
        .await
        .unwrap()
        .0;

    assert_eq!(f(&db, "B2", "RVAL"), 255.0, "busyRecord.c:176-179");
    assert_eq!(f(&db, "BO2", "RVAL"), 1.0, "boRecord.c:170");
    assert_eq!(f(&db, "BO2", "ORAW"), 1.0, "boRecord.c:174");
    assert_eq!(f(&db, "BO2", "MLST"), 1.0, "boRecord.c:172");
}
