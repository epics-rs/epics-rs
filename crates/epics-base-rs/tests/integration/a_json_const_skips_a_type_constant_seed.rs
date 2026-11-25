//! A seed C guards with `plink->type == CONSTANT` skips a `{const:…}` link,
//! which `dbParseLink` types `JSON_LINK` by its braces (`dbStaticLib.c:2280-2287`).
//! An unguarded `recGblInitConstantLink` loads the same link through
//! `lnkConst_loadScalar` (`dbLink.c:234-242`).

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::int64out::Int64outRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn ioc(db: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::new()))
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("calc", || Box::new(CalcRecord::default()))
        .register_record_type("int64out", || Box::new(Int64outRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::new()))
        .register_record_type("sseq", || Box::new(SseqRecord::new()))
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn udf(db: &PvDatabase, rec: &str) -> u8 {
    db.get_record(rec).unwrap().read().common.udf
}

/// `int64outRecord.c:109-111`.
#[epics_macros_rs::epics_test]
async fn int64out_json_dol_does_not_seed_val() {
    let db = ioc(r#"
record(int64out, "I") { field(DTYP, "Soft Channel") field(DOL, "{const:5}") }
record(int64out, "K") { field(DTYP, "Soft Channel") field(DOL, "5") }
"#)
    .await;
    assert_eq!((num(&db, "I.VAL"), udf(&db, "I")), (0.0, 1));
    assert_eq!((num(&db, "K.VAL"), udf(&db, "K")), (5.0, 0));
}

/// `busyRecord.c:151-158`.
#[epics_macros_rs::epics_test]
async fn busy_json_dol_does_not_seed_val() {
    let db = ioc(r#"
record(busy, "B") { field(DTYP, "Soft Channel") field(DOL, "{const:1}") }
record(busy, "K") { field(DTYP, "Soft Channel") field(DOL, "1") }
"#)
    .await;
    assert_eq!((num(&db, "B.VAL"), udf(&db, "B")), (0.0, 1));
    assert_eq!((num(&db, "K.VAL"), udf(&db, "K")), (1.0, 0));
}

/// `busyRecord.c:137-139`.
#[epics_macros_rs::epics_test]
async fn busy_json_siml_does_not_seed_simm() {
    let db = ioc(r#"
record(busy, "B") { field(DTYP, "Soft Channel") field(SIML, "{const:1}") }
record(busy, "K") { field(DTYP, "Soft Channel") field(SIML, "1") }
"#)
    .await;
    assert_eq!(num(&db, "B.SIMM"), 0.0);
    assert_eq!(num(&db, "K.SIMM"), 1.0);
}

/// `swaitRecord.c:663-665`.
#[epics_macros_rs::epics_test]
async fn swait_json_siml_does_not_seed_simm() {
    let db = ioc(r#"
record(swait, "W") { field(SIML, "{const:1}") }
record(swait, "K") { field(SIML, "1") }
"#)
    .await;
    assert_eq!(num(&db, "W.SIMM"), 0.0);
    assert_eq!(num(&db, "K.SIMM"), 1.0);
}

/// `swaitRecord.c:668-670`.
#[epics_macros_rs::epics_test]
async fn swait_json_siol_does_not_seed_sval() {
    let db = ioc(r#"
record(swait, "W") { field(SIOL, "{const:4}") }
record(swait, "K") { field(SIOL, "4") }
"#)
    .await;
    assert_eq!(num(&db, "W.SVAL"), 0.0);
    assert_eq!(num(&db, "K.SVAL"), 4.0);
}

/// `transformRecord.c:444-447`.
#[epics_macros_rs::epics_test]
async fn transform_json_inp_does_not_seed_its_value() {
    let db = ioc(r#"
record(transform, "T") { field(INPA, "{const:2}") field(INPB, "2") }
"#)
    .await;
    assert_eq!((num(&db, "T.A"), num(&db, "T.B")), (0.0, 2.0));
}

/// `aCalcoutRecord.c:210-214`.
#[epics_macros_rs::epics_test]
async fn acalcout_json_inp_does_not_seed_its_value() {
    let db = ioc(r#"
record(acalcout, "AC") { field(INPA, "{const:2}") field(INPB, "2") }
"#)
    .await;
    assert_eq!((num(&db, "AC.A"), num(&db, "AC.B")), (0.0, 2.0));
}

/// `sCalcoutRecord.c:255-259`.
#[epics_macros_rs::epics_test]
async fn scalcout_json_inp_does_not_seed_its_value() {
    let db = ioc(r#"
record(scalcout, "SC") { field(INPA, "{const:2}") field(INPB, "2") }
"#)
    .await;
    assert_eq!((num(&db, "SC.A"), num(&db, "SC.B")), (0.0, 2.0));
}

/// `sseqRecord.c:185-191`.
#[epics_macros_rs::epics_test]
async fn sseq_json_sell_does_not_seed_seln() {
    let db = ioc(r#"
record(sseq, "S") { field(SELL, "{const:2}") }
record(sseq, "K") { field(SELL, "2") }
"#)
    .await;
    assert_eq!(num(&db, "S.SELN"), 1.0);
    assert_eq!(num(&db, "K.SELN"), 2.0);
}

/// `sseqRecord.c:203-205`; the empty STR1 then takes DO1's rendering
/// (`:246-248`).
#[epics_macros_rs::epics_test]
async fn sseq_json_dol_does_not_seed_do_or_str() {
    let db = ioc(r#"
record(sseq, "S") { field(DOL1, "{const:5}") }
record(sseq, "K") { field(DOL1, "5") }
"#)
    .await;
    assert_eq!(num(&db, "S.DO1"), 0.0);
    assert_eq!(db.get_pv("S.STR1").unwrap(), EpicsValue::String("0".into()));
    assert_eq!(num(&db, "K.DO1"), 5.0);
}

/// The boundary: `calcRecord.c:103` is unguarded, so lnkConst loads.
#[epics_macros_rs::epics_test]
async fn calc_json_inp_seeds_its_value() {
    let db = ioc(r#"record(calc, "C") { field(INPA, "{const:2}") }"#).await;
    assert_eq!(num(&db, "C.A"), 2.0);
}

/// A runtime put of `{const:7}` to `rec.INPA`: C's `special()` loads and posts
/// only under `plink->type == CONSTANT`, so `A` keeps its seed and posts nothing.
async fn json_inp_put_neither_loads_nor_posts(rtyp: &str) {
    let db = ioc(&format!(r#"record({rtyp}, "R") {{ field(INPA, "2") }}"#)).await;
    let cell = db.get_record("R").unwrap();
    let mut rx = cell
        .write()
        .add_subscriber("A", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .unwrap();
    db.put_record_field_from_ca("R", "INPA", EpicsValue::String("{const:7}".into()))
        .await
        .unwrap();
    assert_eq!(num(&db, "R.A"), 2.0, "{rtyp}");
    assert!(rx.try_recv().is_err(), "{rtyp}: A was posted");
}

/// `transformRecord.c:715-720`.
#[epics_macros_rs::epics_test]
async fn transform_json_inp_put_neither_loads_nor_posts() {
    json_inp_put_neither_loads_nor_posts("transform").await;
}

/// `aCalcoutRecord.c:533-537`.
#[epics_macros_rs::epics_test]
async fn acalcout_json_inp_put_neither_loads_nor_posts() {
    json_inp_put_neither_loads_nor_posts("acalcout").await;
}

/// `sCalcoutRecord.c:513-517`.
#[epics_macros_rs::epics_test]
async fn scalcout_json_inp_put_neither_loads_nor_posts() {
    json_inp_put_neither_loads_nor_posts("scalcout").await;
}
