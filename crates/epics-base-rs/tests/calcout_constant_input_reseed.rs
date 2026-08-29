//! R19-1 — a runtime put to a CONSTANT `INPn` must re-seed its value field.
//!
//! A constant link delivers NOTHING at process time (`dbConstGetValue`,
//! `dbConstLink.c:219-225`), so the ONLY way `field(INPB,"2")` ever reaches `B`
//! is the init seed. Without a matching re-seed on a runtime put, `caput
//! CO.INPB 7` stores the link text and `B` keeps its `.db` value forever — the
//! put looks accepted and changes nothing.
//!
//! C's INTENT is explicit (`calcoutRecord.c:373-378`, `sCalcoutRecord.c:513-518`,
//! `aCalcoutRecord.c:533-538`): `special()` re-runs `recGblInitConstantLink`,
//! posts the value field with `DBE_VALUE`, and sets `INAV = CON`.
//!
//! C's compiled BEHAVIOUR does not do it — an ordering bug defeats the code:
//! `dbPutFieldLink` (`dbAccess.c:1164-1176`) calls `dbRemoveLink` (which NULLs
//! `plink->lset`, `dbLink.c:207`), then `dbPutSpecial(paddr, 1)`, and only then
//! `dbAddLink` (`:1205`) installs the new lset. So inside `special()` the lset is
//! still NULL, and `recGblInitConstantLink` -> `dbLoadLink` -> `S_db_noLSET`
//! (`dbLink.c:241`, `recGbl.c:175`) returns FALSE without loading anything.
//! Measured on softIoc 7.0.10.1-DEV: `caput -s CO.INPB 7` leaves INPB="7" and
//! B=2. (Same root cause makes C's `dbLinkIsConstant` answer TRUE for a NULL
//! lset, so a re-point to a real PV also leaves INBV stuck at "Constant".)
//!
//! Per the product policy this is Tier 2 with C WRONG: the port implements the
//! INTENDED behaviour and does not reproduce the bug.
//!
//! Boundaries, one case each:
//!   - the new link IS constant -> re-seed + post + the next calc uses it
//!   - the new link is NOT constant -> no re-seed (the link read owns the value)
//!   - a record type whose C `special()` has no such arm (calc: `special()` only
//!     handles SPC_CALC, `calcRecord.c:139-157`) -> no re-seed
//!   - sCalcout/aCalcout NON-numeric inputs, which C's `fieldIndex <= INPL`
//!     guard excludes -> no re-seed

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use std::collections::HashSet;

async fn field(db: &PvDatabase, rec: &str, f: &str) -> EpicsValue {
    db.get_record(rec)
        .unwrap_or_else(|| panic!("{rec} missing"))
        .read()
        .record
        .get_field(f)
        .unwrap_or_else(|| panic!("{rec}.{f} missing"))
}

async fn put(db: &PvDatabase, rec: &str, f: &str, v: &str) {
    db.put_record_field_from_ca(rec, f, EpicsValue::String(v.into()))
        .await
        .unwrap();
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// A calcout with `CALC="A+B"`, `INPA="1"`, `INPB="2"` — both inputs constant.
async fn calcout_db() -> PvDatabase {
    let db = PvDatabase::new();
    let mut co = CalcoutRecord::default();
    co.put_field("CALC", EpicsValue::String("A+B".into()))
        .unwrap();
    co.put_field("INPA", EpicsValue::String("1".into()))
        .unwrap();
    co.put_field("INPB", EpicsValue::String("2".into()))
        .unwrap();
    db.add_record("CO", Box::new(co)).await.unwrap();
    db
}

/// The new link is CONSTANT: the value field takes the constant immediately —
/// no process needed, exactly as the init seed does — and the next calculation
/// uses it.
#[epics_macros_rs::epics_test]
async fn calcout_put_to_a_constant_inp_reseeds_the_value_field() {
    let db = calcout_db().await;
    assert_eq!(field(&db, "CO", "B").await, EpicsValue::Double(2.0));

    put(&db, "CO", "INPB", "7").await;

    assert_eq!(field(&db, "CO", "B").await, EpicsValue::Double(7.0));
    process(&db, "CO").await;
    assert_eq!(field(&db, "CO", "VAL").await, EpicsValue::Double(8.0));
}

/// C posts the re-seeded value field with a literal `DBE_VALUE`
/// (`calcoutRecord.c:376`). Nothing else would: `B` is not `pp(TRUE)`, and the
/// put was to `INPB`.
#[epics_macros_rs::epics_test]
async fn calcout_reseed_posts_the_value_field() {
    let db = calcout_db().await;
    let inst = db.get_record("CO").unwrap();
    let mut b_rx = inst
        .write()
        .add_subscriber("B", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .expect("a B subscription must be accepted");

    put(&db, "CO", "INPB", "7").await;

    let event = b_rx.try_recv().expect("the re-seed must post B");
    assert_eq!(event.snapshot.value, EpicsValue::Double(7.0));
}

/// The new link is NOT constant: `recGblInitConstantLink` returns FALSE and
/// touches nothing (C `dbLoadLink` is a constant-link-only lset entry). The
/// value field keeps what it had until the LINK delivers — the process-time read
/// owns it from here on.
#[epics_macros_rs::epics_test]
async fn calcout_put_of_a_pv_link_does_not_reseed() {
    let db = calcout_db().await;
    let mut src = CalcRecord::new("0");
    src.put_field("VAL", EpicsValue::Double(41.0)).unwrap();
    db.add_record("SRC", Box::new(src)).await.unwrap();

    put(&db, "CO", "INPB", "SRC").await;

    // Not re-seeded — B still holds the old constant.
    assert_eq!(field(&db, "CO", "B").await, EpicsValue::Double(2.0));
    // And the link now owns it: the next process reads SRC.
    process(&db, "CO").await;
    assert_eq!(field(&db, "CO", "B").await, EpicsValue::Double(41.0));
}

/// C's `calcRecord::special` (`calcRecord.c:139-157`) handles ONLY `SPC_CALC` —
/// calc's INPA..INPL are not `special(SPC_MOD)` at all, so a runtime put to a
/// constant INPn re-seeds nothing. calc must NOT inherit calcout's behaviour.
#[epics_macros_rs::epics_test]
async fn calc_put_to_a_constant_inp_does_not_reseed() {
    let db = PvDatabase::new();
    let mut c = CalcRecord::new("A+B");
    c.put_field("INPA", EpicsValue::String("1".into())).unwrap();
    c.put_field("INPB", EpicsValue::String("2".into())).unwrap();
    db.add_record("C1", Box::new(c)).await.unwrap();

    put(&db, "C1", "INPB", "7").await;

    assert_eq!(field(&db, "C1", "B").await, EpicsValue::Double(2.0));
    process(&db, "C1").await;
    assert_eq!(field(&db, "C1", "VAL").await, EpicsValue::Double(3.0));
}

/// sCalcout: the NUMERIC input re-seeds (C `sCalcoutRecord.c:514-516`), the
/// STRING input does not — C guards the load with
/// `if (fieldIndex <= scalcoutRecordINPL)`, and `pvalue = &pcalc->a + lnkIndex`
/// is a `double *` that cannot address AA at all.
#[epics_macros_rs::epics_test]
async fn scalcout_reseeds_numeric_inputs_only() {
    let db = PvDatabase::new();
    let mut sc = ScalcoutRecord::new();
    sc.put_field("CALC", EpicsValue::String("A+B".into()))
        .unwrap();
    sc.put_field("INPB", EpicsValue::String("2".into()))
        .unwrap();
    sc.put_field("INAA", EpicsValue::String("xx".into()))
        .unwrap();
    db.add_record("SC", Box::new(sc)).await.unwrap();

    put(&db, "SC", "INPB", "7").await;
    put(&db, "SC", "INAA", "yy").await;

    assert_eq!(field(&db, "SC", "B").await, EpicsValue::Double(7.0));
    assert_eq!(
        field(&db, "SC", "AA").await,
        EpicsValue::String("".into()),
        "C's fieldIndex <= INPL guard excludes the string inputs from the re-seed"
    );
}

/// aCalcout: same guard (`aCalcoutRecord.c:534`) — the numeric input re-seeds,
/// the ARRAY input does not.
#[epics_macros_rs::epics_test]
async fn acalcout_reseeds_numeric_inputs_only() {
    let db = PvDatabase::new();
    let mut ac = AcalcoutRecord::new();
    ac.put_field("NELM", EpicsValue::ULong(4)).unwrap();
    ac.put_field("CALC", EpicsValue::String("A+B".into()))
        .unwrap();
    ac.put_field("INPB", EpicsValue::String("2".into()))
        .unwrap();
    db.add_record("AC", Box::new(ac)).await.unwrap();

    put(&db, "AC", "INPB", "7").await;
    put(&db, "AC", "INAA", "3").await;

    assert_eq!(field(&db, "AC", "B").await, EpicsValue::Double(7.0));
    assert_eq!(
        field(&db, "AC", "AA").await,
        EpicsValue::DoubleArray(vec![0.0; 4]),
        "C's fieldIndex <= INPL guard excludes the array inputs from the re-seed"
    );
}

/// transform: the FOURTH record whose C `special()` re-seeds
/// (`transformRecord.c:715-723`) — the same defect family, found by searching
/// the anchor (`recGblInitConstantLink` inside a `special()` body) across every
/// C record in base and synApps calc. Its post carries `DBE_VALUE | DBE_LOG`
/// (`:719`), unlike the calcout family's bare `DBE_VALUE`.
#[epics_macros_rs::epics_test]
async fn transform_put_to_a_constant_inp_reseeds_and_posts_value_log() {
    use epics_base_rs::server::records::transform::TransformRecord;

    let db = PvDatabase::new();
    let mut t = TransformRecord::default();
    t.put_field("INPB", EpicsValue::String("2".into())).unwrap();
    db.add_record("T", Box::new(t)).await.unwrap();
    assert_eq!(field(&db, "T", "B").await, EpicsValue::Double(2.0));

    let inst = db.get_record("T").unwrap();
    let mut b_rx = inst
        .write()
        .add_subscriber(
            "B",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("a B subscription must be accepted");

    put(&db, "T", "INPB", "7").await;

    assert_eq!(field(&db, "T", "B").await, EpicsValue::Double(7.0));
    let event = b_rx.try_recv().expect("the re-seed must post B");
    assert_eq!(event.snapshot.value, EpicsValue::Double(7.0));
}
