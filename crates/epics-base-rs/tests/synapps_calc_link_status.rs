//! Boundary tests for the link-connection-status diagnostics of the three
//! synApps calc-family records — `transform` (`IAV..IPV`/`OAV..OPV`,
//! `menu(transformIAV)`), `scalcout` and `acalcout`
//! (`INAV..INLV`/`IAAV..ILLV`/`OUTV`, `menu(scalcoutINAV)` /
//! `menu(acalcoutINAV)`).
//!
//! All three C records classify every link in `init_record`
//! (`transformRecord.c:444-473`, `sCalcoutRecord.c:254-288`,
//! `aCalcoutRecord.c:209-243`) and re-classify the one link a put re-points in
//! `special()` (`:709-742`, `:508-569`, `:528-569`). The port served a literal
//! `Constant` from `get_field` for every one of those fields on every record,
//! so a local DB link read `Constant` and an unresolvable name read `Constant`
//! too — the field could not distinguish a wired link from an unwired one,
//! which is the entire purpose an operator opens it for.
//!
//! The classification RULE (`link_status::classify_link`) is shared with
//! `calcout`/`sseq` and covered there. What is pinned here is per-record
//! wiring, by boundary:
//!
//!   * link class — resolvable local DB name (`Local PV`), empty/constant
//!     (`Constant`), unresolvable name (`Ext PV NC`);
//!   * link ROLE — an OUT/OUTx link is classified as an output, from the
//!     output link field, not from the input at the same channel index;
//!   * the array/string input half (`IAAV..ILLV`) is classified from
//!     `INAA..INLL`, not from the scalar `INPA..INPL` at the same index;
//!   * re-classification on a put — the status follows the link the put just
//!     stored, in both directions (wired → unwired and back).

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

// menu(transformIAV) == menu(scalcoutINAV) == menu(acalcoutINAV), all four
// choices in the same order.
const EXT_NC: i16 = 0;
const LOC: i16 = 2;
const CON: i16 = 3;

type Db = Arc<PvDatabase>;

async fn build(db_text: &str) -> Db {
    IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn read(db: &Db, pv: &str) -> i16 {
    db.get_pv(pv)
        .ok()
        .and_then(|v| v.to_f64())
        .map(|f| f as i16)
        .unwrap_or_else(|| panic!("{pv} not readable as a number"))
}

/// The classification is published by a task scheduled through the database's
/// `iocInit` owner, so a status is asserted only after it settles.
async fn poll(db: &Db, pv: &str, want: i16, label: &str) {
    for _ in 0..400 {
        if read(db, pv) == want {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{label}: {pv} != {want} (is {})", read(db, pv));
}

const TRANSFORM_DB: &str = r#"
record(ao, "T:TGT") { }
record(transform, "T") {
    field(INPA, "T:TGT.VAL")
    field(INPC, "T:NOSUCH.VAL")
    field(OUTA, "T:TGT.VAL")
    field(OUTC, "T:NOSUCH.VAL")
}
"#;

/// transform: the three link classes, on both the input and the output half.
#[epics_macros_rs::epics_test]
async fn transform_classifies_every_link_class_on_both_halves() {
    let db = build(TRANSFORM_DB).await;

    poll(&db, "T.IAV", LOC, "INPA is a local record").await;
    assert_eq!(read(&db, "T.IBV"), CON, "INPB is unset");
    assert_eq!(read(&db, "T.ICV"), EXT_NC, "INPC names no record here");

    poll(&db, "T.OAV", LOC, "OUTA is a local record").await;
    assert_eq!(read(&db, "T.OBV"), CON, "OUTB is unset");
    assert_eq!(read(&db, "T.OCV"), EXT_NC, "OUTC names no record here");
}

/// transform: the OUTx half is classified from OUTx, not from INPx at the same
/// channel. Channel D is wired on ONE side only, in each direction.
#[epics_macros_rs::epics_test]
async fn transform_reads_each_half_from_its_own_link() {
    let db = build(
        r#"
record(ao, "T2:TGT") { }
record(transform, "T2") {
    field(INPD, "T2:TGT.VAL")
    field(OUTE, "T2:TGT.VAL")
}
"#,
    )
    .await;

    poll(&db, "T2.IDV", LOC, "INPD wired").await;
    poll(&db, "T2.OEV", LOC, "OUTE wired").await;
    assert_eq!(read(&db, "T2.ODV"), CON, "OUTD is unset though INPD is not");
    assert_eq!(read(&db, "T2.IEV"), CON, "INPE is unset though OUTE is not");
}

/// transform `special()`: the status follows the link a put just stored, in
/// both directions.
#[epics_macros_rs::epics_test]
async fn transform_reclassifies_a_link_a_put_repointed() {
    let db = build(TRANSFORM_DB).await;
    poll(&db, "T.IBV", CON, "INPB starts unset").await;

    db.put_record_field_from_ca("T", "INPB", EpicsValue::String("T:TGT.VAL".into()))
        .await
        .unwrap();
    poll(&db, "T.IBV", LOC, "INPB re-pointed at a local record").await;

    db.put_record_field_from_ca("T", "INPB", EpicsValue::String("".into()))
        .await
        .unwrap();
    poll(&db, "T.IBV", CON, "INPB cleared again").await;
}

const SCALCOUT_DB: &str = r#"
record(ao, "S:TGT") { }
record(scalcout, "S") {
    field(INPA, "S:TGT.VAL")
    field(INPC, "S:NOSUCH.VAL")
    field(INBB, "S:TGT.VAL")
    field(OUT,  "S:TGT.VAL")
}
"#;

/// scalcout: numeric inputs, string inputs and the output link each classify
/// from their own field.
#[epics_macros_rs::epics_test]
async fn scalcout_classifies_numeric_string_and_output_links() {
    let db = build(SCALCOUT_DB).await;

    poll(&db, "S.INAV", LOC, "INPA is a local record").await;
    assert_eq!(read(&db, "S.INBV"), CON, "INPB is unset");
    assert_eq!(read(&db, "S.INCV"), EXT_NC, "INPC names no record here");

    // The string half is INAA..INLL, at its own index: INBB is wired, INAA is
    // not — the opposite of the numeric half at the same two indices.
    poll(&db, "S.IBBV", LOC, "INBB is a local record").await;
    assert_eq!(read(&db, "S.IAAV"), CON, "INAA is unset though INPA is not");

    poll(&db, "S.OUTV", LOC, "OUT is a local record").await;
}

/// scalcout `special()`: C's re-classification case list covers OUT as well as
/// the input links (`sCalcoutRecord.c:495-509`).
#[epics_macros_rs::epics_test]
async fn scalcout_reclassifies_out_and_string_links_on_a_put() {
    let db = build(SCALCOUT_DB).await;

    db.put_record_field_from_ca("S", "OUT", EpicsValue::String("S:NOSUCH.VAL".into()))
        .await
        .unwrap();
    poll(&db, "S.OUTV", EXT_NC, "OUT re-pointed at nothing here").await;

    db.put_record_field_from_ca("S", "INAA", EpicsValue::String("S:TGT.VAL".into()))
        .await
        .unwrap();
    poll(&db, "S.IAAV", LOC, "INAA re-pointed at a local record").await;
}

const ACALCOUT_DB: &str = r#"
record(ao, "A:TGT") { }
record(acalcout, "A") {
    field(INPA, "A:TGT.VAL")
    field(INPC, "A:NOSUCH.VAL")
    field(INBB, "A:TGT.VAL")
    field(OUT,  "A:TGT.VAL")
}
"#;

/// acalcout: numeric inputs, ARRAY inputs and the output link each classify
/// from their own field.
#[epics_macros_rs::epics_test]
async fn acalcout_classifies_numeric_array_and_output_links() {
    let db = build(ACALCOUT_DB).await;

    poll(&db, "A.INAV", LOC, "INPA is a local record").await;
    assert_eq!(read(&db, "A.INBV"), CON, "INPB is unset");
    assert_eq!(read(&db, "A.INCV"), EXT_NC, "INPC names no record here");

    poll(&db, "A.IBBV", LOC, "INBB is a local record").await;
    assert_eq!(read(&db, "A.IAAV"), CON, "INAA is unset though INPA is not");

    poll(&db, "A.OUTV", LOC, "OUT is a local record").await;
}

/// acalcout `special()`: same case list as scalcout
/// (`aCalcoutRecord.c:503-533`), including OUT.
#[epics_macros_rs::epics_test]
async fn acalcout_reclassifies_out_and_array_links_on_a_put() {
    let db = build(ACALCOUT_DB).await;

    db.put_record_field_from_ca("A", "OUT", EpicsValue::String("A:NOSUCH.VAL".into()))
        .await
        .unwrap();
    poll(&db, "A.OUTV", EXT_NC, "OUT re-pointed at nothing here").await;

    db.put_record_field_from_ca("A", "INAA", EpicsValue::String("A:TGT.VAL".into()))
        .await
        .unwrap();
    poll(&db, "A.IAAV", LOC, "INAA re-pointed at a local record").await;
}

/// A record whose links are all unset stays at C's post-`init_record` value on
/// every one of the three records — the case the port already served, and the
/// one the live classification must not regress.
#[epics_macros_rs::epics_test]
async fn an_unwired_record_still_reads_constant_everywhere() {
    let db = build(
        r#"
record(transform, "U:T") { }
record(scalcout,  "U:S") { }
record(acalcout,  "U:A") { }
"#,
    )
    .await;

    for pv in ["U:T.IAV", "U:T.IPV", "U:T.OAV", "U:T.OPV"] {
        poll(&db, pv, CON, "transform unwired").await;
    }
    for pv in ["U:S.INAV", "U:S.ILLV", "U:S.OUTV"] {
        poll(&db, pv, CON, "scalcout unwired").await;
    }
    for pv in ["U:A.INAV", "U:A.ILLV", "U:A.OUTV"] {
        poll(&db, pv, CON, "acalcout unwired").await;
    }
}
