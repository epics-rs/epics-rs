//! R9-69 — a failed INPn read skips the sub/aSub subroutine and stops the
//! input fetch dead at that link.
//!
//! C `subRecord.c::fetch_values` (407-418):
//!
//! ```c
//! for (i = 0; i < INP_ARG_MAX; i++, plink++, pvalue++) {
//!     if (dbGetLink(plink, DBR_DOUBLE, pvalue, 0, 0))
//!         return -1;
//! }
//! ```
//!
//! and `subRecord.c::process` (145-146):
//!
//! ```c
//! status = fetch_values(prec);
//! if (status == 0) status = do_sub(prec);
//! ```
//!
//! Two observable consequences, both of which the port lacked — it fetched
//! every link and ran SNAM unconditionally:
//!
//! 1. the subroutine does not run, so VAL (and aSub's VALA..VALU) freeze and
//!    `do_sub`'s `udf = isnan(val)` / BAD_SUB / SOFT alarms never happen;
//! 2. the inputs *behind* the failed link are never read this cycle and keep
//!    their previous values.
//!
//! `aSubRecord.c` (fetch 277-289, process 216-218) has the identical shape.

use std::collections::HashSet;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Record, SubroutineFn};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::sub_record::SubRecord;
use epics_base_rs::types::EpicsValue;

/// `VAL = A + C` — proves both that the subroutine ran and which inputs it saw.
fn sum_a_c() -> SubroutineFn {
    Box::new(|rec: &mut dyn Record| {
        let a = rec.get_field("A").and_then(|v| v.to_f64()).unwrap_or(0.0);
        let c = rec.get_field("C").and_then(|v| v.to_f64()).unwrap_or(0.0);
        rec.put_field("VAL", EpicsValue::Double(a + c))?;
        Ok(0)
    })
}

/// Build a `sub` with SNAM bound to [`sum_a_c`], INPA→SRCA, INPC→SRCC,
/// VAL seeded to -1 and C seeded to 99. `inpb` is the INPB link string.
async fn sub_db(inpb: &str) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SRCA", Box::new(AiRecord::new(10.0)))
        .await
        .unwrap();
    db.add_record("SRCC", Box::new(AiRecord::new(3.0)))
        .await
        .unwrap();
    // SNAM before the add: C applies every `field()` line and only then runs
    // `init_record`, which parks PACT for good when SNAM is empty
    // (subRecord.c:119-123).
    let mut seed = SubRecord::default();
    seed.put_field("SNAM", EpicsValue::String("sum_a_c".into()))
        .unwrap();
    db.add_record("SUB", Box::new(seed)).await.unwrap();

    let arc = db.get_record("SUB").unwrap();
    let mut inst = arc.write();
    let r = &mut inst.record;
    r.put_field("INPA", EpicsValue::String("SRCA".into()))
        .unwrap();
    if !inpb.is_empty() {
        r.put_field("INPB", EpicsValue::String(inpb.into()))
            .unwrap();
    }
    r.put_field("INPC", EpicsValue::String("SRCC".into()))
        .unwrap();
    // Stale "previous cycle" values the C gate must preserve.
    r.put_field("C", EpicsValue::Double(99.0)).unwrap();
    r.put_field("VAL", EpicsValue::Double(-1.0)).unwrap();
    inst.subroutine = Some(Arc::new(sum_a_c()));
    drop(inst);
    db
}

#[epics_macros_rs::epics_test]
async fn r9_69_failed_inpn_read_skips_the_subroutine_and_freezes_val() {
    // INPB names a PV that does not exist: C's `dbGetLink` fails here and
    // `fetch_values` returns -1 before ever reaching INPC.
    let db = sub_db("NOSUCHPV").await;

    let mut v = HashSet::new();
    db.process_record_with_links("SUB", &mut v, 0)
        .await
        .unwrap();

    let arc = db.get_record("SUB").unwrap();
    let inst = arc.read();
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Double(-1.0)),
        "a failed INPB read means C never calls do_sub: VAL freezes at -1, \
         it is not recomputed to A+C"
    );
    assert_eq!(
        inst.record.get_field("A"),
        Some(EpicsValue::Double(10.0)),
        "INPA is fetched before the failure — C's loop reached it"
    );
    assert_eq!(
        inst.record.get_field("C"),
        Some(EpicsValue::Double(99.0)),
        "INPC sits behind the failed INPB: C `return -1`s first, so C is \
         never read and keeps its previous value"
    );
}

/// An *unset* INPB is a CONSTANT link, not a failure — C `dbGetLink` returns
/// success for it, so the subroutine still runs. Guards the gate against
/// arming on "no link configured".
#[epics_macros_rs::epics_test]
async fn r9_69_unset_link_is_not_a_fetch_failure() {
    let db = sub_db("").await;

    let mut v = HashSet::new();
    db.process_record_with_links("SUB", &mut v, 0)
        .await
        .unwrap();

    let arc = db.get_record("SUB").unwrap();
    let inst = arc.read();
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Double(13.0)),
        "every configured link resolved: do_sub runs and VAL = A + C = 10 + 3"
    );
}

/// aSub is the same C shape (`aSubRecord.c::fetch_values` 277-289 returns on
/// the first failure; `process` 216-218 gates `do_sub`). Its VAL is the
/// subroutine's return status, so a skipped run leaves VAL untouched.
#[epics_macros_rs::epics_test]
async fn r9_69_asub_failed_inpn_read_skips_the_subroutine() {
    let db = PvDatabase::new();
    db.add_record("SRCA", Box::new(AiRecord::new(10.0)))
        .await
        .unwrap();
    db.add_record("ASUB", Box::new(ASubRecord::default()))
        .await
        .unwrap();

    {
        let arc = db.get_record("ASUB").unwrap();
        let mut inst = arc.write();
        let r = &mut inst.record;
        r.put_field("SNAM", EpicsValue::String("ran".into()))
            .unwrap();
        r.put_field("INPA", EpicsValue::String("SRCA".into()))
            .unwrap();
        r.put_field("INPB", EpicsValue::String("NOSUCHPV".into()))
            .unwrap();
        r.put_field("VAL", EpicsValue::Double(-1.0)).unwrap();
        // Returns 7: had the subroutine run, the framework would publish the
        // status as aSub's VAL (C `aSubRecord.c:223` `prec->val = status`).
        let sub_fn: SubroutineFn = Box::new(|_rec: &mut dyn Record| Ok(7));
        inst.subroutine = Some(Arc::new(sub_fn));
    }

    let mut v = HashSet::new();
    db.process_record_with_links("ASUB", &mut v, 0)
        .await
        .unwrap();

    let arc = db.get_record("ASUB").unwrap();
    let inst = arc.read();
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Long(-1)),
        "a failed INPB read skips do_sub, so VAL keeps -1 and never becomes \
         the subroutine's status 7"
    );
}
