//! dfanout `OUTn` and fanout `LNKn` are read through the record's link cache:
//! parsed at load, re-parsed only after a put of that link field, as C parses
//! at `dbInitLink` and `dbPutFieldLink`. Every expectation is
//! softIoc's `dbgf` on the same database and the same puts.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(dfanout, "DF") { field(VAL, "3") field(OUTA, "T1 NPP") field(OUTB, "T2 NPP") }
record(dfanout, "DS") { field(VAL, "4") field(SELM, "Specified") field(SELN, "2") field(OUTA, "S1 NPP") field(OUTB, "S2 NPP") }
record(ao, "T1") { }
record(ao, "T2") { }
record(ao, "T3") { }
record(ao, "S1") { }
record(ao, "S2") { }
record(fanout, "FO") { field(LNK1, "C1") }
record(calc, "C1") { field(CALC, "VAL+1") }
record(calc, "C2") { field(CALC, "VAL+1") }
"#;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build() -> Db {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn val(db: &Db, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn put_link(db: &Db, rec: &str, field: &str, text: &str) {
    db.put_record_field_from_ca(rec, field, EpicsValue::String(PvString::from(text)))
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn dfanout_writes_every_set_out_link() {
    let db = build().await;
    db.process_record("DF").await.unwrap();
    assert_eq!((val(&db, "T1"), val(&db, "T2")), (3.0, 3.0));
}

/// `SELM Specified, SELN 2` drives OUTB alone.
#[epics_macros_rs::epics_test]
async fn dfanout_specified_drives_only_the_selected_link() {
    let db = build().await;
    db.process_record("DS").await.unwrap();
    assert_eq!((val(&db, "S1"), val(&db, "S2")), (0.0, 4.0));
}

/// A put of OUTA after a cycle has read it moves the next write: the cached
/// parse is the old text's until the put drops it.
#[epics_macros_rs::epics_test]
async fn a_put_of_an_out_link_moves_the_next_write() {
    let db = build().await;
    db.process_record("DF").await.unwrap();
    put_link(&db, "DF", "OUTA", "T3 NPP").await;
    db.put_pv("DF", EpicsValue::Double(5.0)).await.unwrap();
    db.process_record("DF").await.unwrap();
    assert_eq!((val(&db, "T1"), val(&db, "T3")), (3.0, 5.0));
}

/// The fanout twin: after `LNK1` is put, the next cycle processes the new
/// target and not the old one.
#[epics_macros_rs::epics_test]
async fn a_put_of_a_forward_link_moves_the_next_process() {
    let db = build().await;
    db.process_record("FO").await.unwrap();
    assert_eq!(val(&db, "C1"), 1.0);
    put_link(&db, "FO", "LNK1", "C2").await;
    db.process_record("FO").await.unwrap();
    assert_eq!((val(&db, "C1"), val(&db, "C2")), (1.0, 1.0));
}

/// softIoc: a put of OUTA and LNK1 before either record's first cycle —
/// T1 0, T3 3, C1 0, C2 1: the load-time parse is replaced, not read.
#[epics_macros_rs::epics_test]
async fn a_put_before_the_first_cycle_moves_it() {
    let db = build().await;
    put_link(&db, "DF", "OUTA", "T3 NPP").await;
    db.process_record("DF").await.unwrap();
    put_link(&db, "FO", "LNK1", "C2").await;
    db.process_record("FO").await.unwrap();
    assert_eq!(
        (
            val(&db, "T1"),
            val(&db, "T3"),
            val(&db, "C1"),
            val(&db, "C2")
        ),
        (0.0, 3.0, 0.0, 1.0)
    );
}

const SELL_DB: &str = r#"
record(dfanout, "DL") { field(VAL, "6") field(SELM, "Specified") field(SELL, "SEL") field(OUTA, "U1 NPP") field(OUTB, "U2 NPP") }
record(longout, "SEL") { field(VAL, "2") }
record(longout, "SEL2") { field(VAL, "1") }
record(ao, "U1") { }
record(ao, "U2") { }
"#;

/// softIoc: SELL reads SEL (2) and drives OUTB; after `dbpf DL.SELL SEL2`
/// the next cycle reads 1 and drives OUTA.
#[epics_macros_rs::epics_test]
async fn a_put_of_sell_moves_the_next_selection() {
    let db = IocBuilder::new()
        .db_string(SELL_DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.process_record("DL").await.unwrap();
    assert_eq!(
        (val(&db, "U1"), val(&db, "U2"), val(&db, "DL.SELN")),
        (0.0, 6.0, 2.0)
    );
    put_link(&db, "DL", "SELL", "SEL2").await;
    db.put_pv("DL", EpicsValue::Double(7.0)).await.unwrap();
    db.process_record("DL").await.unwrap();
    assert_eq!(
        (val(&db, "U1"), val(&db, "U2"), val(&db, "DL.SELN")),
        (7.0, 6.0, 1.0)
    );
}
