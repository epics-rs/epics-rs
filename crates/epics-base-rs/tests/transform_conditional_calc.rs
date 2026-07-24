//! R19-86 — transform's conditional-calc gate is C's, term for term.
//!
//! synApps `transformRecord.c:571-591`:
//!
//! ```c
//! no_inlink   = plink->type == CONSTANT;
//! same        = (*pval==0. && *plval==0.) || ((pu[0]==plu[0]) && (pu[1]==plu[1]));
//! new_value   = (!same || ((ptran->map & (1<<i)) != 0));
//! postfix_ok  = *pclcbuf && (*prpcbuf != BAD_EXPRESSION);
//! if (((no_inlink && !new_value) || ptran->copt==transformCOPT_ALWAYS) && postfix_ok)
//! ```
//!
//! The port had rewritten two of the four terms:
//!   * `no_inlink` as "the INPx text is EMPTY" — but `field(INPA,"2")` is a
//!     CONSTANT link, so a constant-seeded channel looked link-driven and its
//!     CLCx was NEVER evaluated in the default Conditional mode.
//!   * `same` as "no put landed on this channel" — i.e. the `map` bit alone,
//!     with the value-vs-LA comparison dropped. That misses every write that
//!     does not come through `special()`, and R19-1 added one: the CONSTANT-INPx
//!     re-seed (`:717`).
//!
//! One case per boundary of the gate.

// RTEMS-EXEC-MODEL-ALLOW(7): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;
use std::collections::HashSet;

async fn field(db: &PvDatabase, rec: &str, f: &str) -> f64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field(f)
        .unwrap_or_else(|| panic!("T.{f} missing"))
        .to_f64()
        .unwrap()
}

async fn put(db: &PvDatabase, rec: &str, f: &str, v: EpicsValue) {
    db.put_record_field_from_ca(rec, f, v).await.unwrap();
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// `add_record` runs the init passes, the constant seed and the seed tail, so a
/// record built here enters its first cycle exactly as one loaded from a .db.
async fn transform_db(fields: &[(&str, EpicsValue)]) -> PvDatabase {
    let db = PvDatabase::new();
    let mut t = TransformRecord::default();
    for (f, v) in fields {
        t.put_field(f, v.clone()).unwrap();
    }
    db.add_record("T", Box::new(t)).await.unwrap();
    db
}

fn s(v: &str) -> EpicsValue {
    EpicsValue::String(v.into())
}

/// `no_inlink` — a CONSTANT link is a constant link whether or not it carries a
/// number. `field(INPA,"2")` seeds A=2 and leaves the channel UNLINKED, so
/// Conditional mode evaluates CLCA every cycle. (The port used to read the
/// text's emptiness: A sat at 2 forever.)
#[tokio::test]
async fn a_constant_valued_inp_link_is_no_inlink_so_the_channel_still_calcs() {
    let db = transform_db(&[("INPA", s("2")), ("CLCA", s("A+1"))]).await;
    assert_eq!(field(&db, "T", "A").await, 2.0, "the init seed");

    process(&db, "T").await;
    assert_eq!(field(&db, "T", "A").await, 3.0);
    process(&db, "T").await;
    assert_eq!(field(&db, "T", "A").await, 4.0);
}

/// `map` bit — a put to the VALUE field itself marks the channel new
/// (`:698-704`), so the cycle that put drives (`A` is `pp(TRUE)`,
/// `transformRecord.dbd:409-414`, so `dbPutField` processes the record) leaves
/// the value alone. `:600` clears the map, so the NEXT cycle calculates again:
/// the put survives exactly one cycle.
#[tokio::test]
async fn a_put_to_the_value_field_suppresses_one_cycle_then_calc_resumes() {
    let db = transform_db(&[("CLCA", s("A+1"))]).await;

    put(&db, "T", "A", EpicsValue::Double(10.0)).await;
    assert_eq!(
        field(&db, "T", "A").await,
        10.0,
        "the pp(TRUE) cycle sees the map bit and does not recalculate"
    );

    process(&db, "T").await;
    assert_eq!(field(&db, "T", "A").await, 11.0, "map cleared at :600");
}

/// `same` — the CONSTANT-INPx re-seed (R19-1, `:717`) writes A without going
/// through the value field's `special()`, so NO map bit is set. It is `!same`
/// (A != LA) that makes the channel new and stops CLCA from overwriting the
/// value the operator just seeded. This is the case the port's `map`-only gate
/// got wrong: it recalculated immediately and the re-seed was invisible.
#[tokio::test]
async fn a_constant_inp_reseed_is_new_by_the_same_test_not_by_the_map_bit() {
    let db = transform_db(&[("INPA", s("2")), ("CLCA", s("A+1"))]).await;

    put(&db, "T", "INPA", EpicsValue::String("50".into())).await;
    assert_eq!(field(&db, "T", "A").await, 50.0, "re-seeded");

    process(&db, "T").await;
    assert_eq!(
        field(&db, "T", "A").await,
        50.0,
        "A != LA -> new -> no calc"
    );

    // `monitor()` has now committed LA = 50, so A is no longer new.
    process(&db, "T").await;
    assert_eq!(field(&db, "T", "A").await, 51.0);
}

/// `same`'s zero carve-out — `(*pval==0. && *plval==0.)`. A re-seed of `-0`
/// gives A a bit pattern that DIFFERS from LA's `+0`, and the raw bitwise
/// compare would call the channel new. C's first clause overrides exactly that:
/// two zeroes of any sign are the same, so the channel is NOT new and CLCA runs
/// on the very next cycle.
#[tokio::test]
async fn a_reseed_of_negative_zero_is_same_as_positive_zero_so_calc_is_not_suppressed() {
    let db = transform_db(&[("CLCA", s("A+1"))]).await;
    assert_eq!(field(&db, "T", "A").await, 0.0);

    put(&db, "T", "INPA", EpicsValue::String("-0".into())).await;
    assert!(
        field(&db, "T", "A").await.is_sign_negative(),
        "the re-seed must actually land -0.0, or this test proves nothing"
    );

    process(&db, "T").await;
    assert_eq!(field(&db, "T", "A").await, 1.0);
}

/// `no_inlink` false — a channel driven by a real PV link is never calculated in
/// Conditional mode, whatever its value did.
#[tokio::test]
async fn a_pv_linked_channel_is_not_calculated_in_conditional_mode() {
    let db = transform_db(&[("INPA", s("SRC")), ("CLCA", s("A+1"))]).await;
    let mut src = TransformRecord::default();
    src.put_field("VAL", EpicsValue::Double(7.0)).unwrap();
    db.add_record("SRC", Box::new(src)).await.unwrap();

    process(&db, "T").await;
    assert_eq!(field(&db, "T", "A").await, 7.0, "the link owns A, not CLCA");
}

/// `copt == ALWAYS` — the other arm of the gate. It overrides BOTH `no_inlink`
/// and `new_value`: a linked channel, and a channel that was just put, are
/// calculated anyway.
#[tokio::test]
async fn copt_always_calculates_a_linked_channel_and_a_freshly_put_one() {
    let db = transform_db(&[
        ("COPT", EpicsValue::Short(1)),
        ("INPA", s("SRC")),
        ("CLCA", s("A+1")),
        ("CLCB", s("B+1")),
    ])
    .await;
    let mut src = TransformRecord::default();
    src.put_field("VAL", EpicsValue::Double(7.0)).unwrap();
    db.add_record("SRC", Box::new(src)).await.unwrap();

    // `B` is `pp(TRUE)`, so this put processes the record itself.
    put(&db, "T", "B", EpicsValue::Double(10.0)).await;

    assert_eq!(field(&db, "T", "A").await, 8.0, "linked, but COPT=Always");
    assert_eq!(field(&db, "T", "B").await, 11.0, "map bit, but COPT=Always");
}

/// `postfix_ok` — an EMPTY CLCx is never evaluated (`*pclcbuf` is false), so an
/// unlinked channel with no expression keeps its value forever. It is the
/// passthrough shape: INPx -> A -> OUTx with no calc at all.
#[tokio::test]
async fn an_empty_clc_is_never_evaluated() {
    let db = transform_db(&[("INPA", s("2"))]).await;

    process(&db, "T").await;
    process(&db, "T").await;
    assert_eq!(field(&db, "T", "A").await, 2.0);
}
