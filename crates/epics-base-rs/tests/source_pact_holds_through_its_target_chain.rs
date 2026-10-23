//! C `processTarget` sets `psrc->pact` while the target processes and puts it
//! back after (`dbDbLink.c:457`, `:514`), on the PP put, the PP read and the
//! FLNK and fanout hops alike. A record in that chain reading `<src>.PACT`
//! sees 1, so `SDIS "<src>.PACT"` disables it. softIoc: `ao A OUT "B PP"`,
//! `B FLNK C`, `C SDIS "A.PACT"` leaves C DISABLE.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "A") { field(OUT, "B PP") }
record(ao, "B") { field(FLNK, "C") }
record(calc, "C") { field(CALC, "VAL+1") field(SDIS, "A.PACT") }

record(ao, "F") { field(FLNK, "G") }
record(calc, "G") { field(CALC, "VAL+1") field(SDIS, "F.PACT") }

record(calc, "R") { field(INPA, "S PP") field(CALC, "A") }
record(calc, "S") { field(CALC, "VAL+1") field(SDIS, "R.PACT") }

record(fanout, "FO") { field(LNK1, "T1") }
record(calc, "T1") { field(CALC, "VAL+1") field(SDIS, "FO.PACT") }
"#;

async fn ioc() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// menuAlarmStat DISABLE.
const DISABLE: f64 = 18.0;

#[epics_macros_rs::epics_test]
async fn a_pp_put_holds_the_writer_pact_down_the_targets_forward_link() {
    let db = ioc().await;
    db.put_record_field_from_ca("A", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(num(&db, "B"), 1.0, "the PP put processed B");
    assert_eq!(num(&db, "C"), 0.0, "C did not process");
    assert_eq!(num(&db, "C.STAT"), DISABLE);
    assert_eq!(num(&db, "A.PACT"), 0.0, "put back after the chain");

    db.process_record("C").await.unwrap();
    assert_eq!(num(&db, "C"), 1.0, "outside A's chain A.PACT reads 0");
}

#[epics_macros_rs::epics_test]
async fn a_forward_link_holds_the_linking_record_pact() {
    let db = ioc().await;
    db.process_record("F").await.unwrap();
    assert_eq!(num(&db, "G"), 0.0);
    assert_eq!(num(&db, "G.STAT"), DISABLE);
    assert_eq!(num(&db, "F.PACT"), 0.0);
}

#[epics_macros_rs::epics_test]
async fn a_pp_read_holds_the_reader_pact() {
    let db = ioc().await;
    db.process_record("R").await.unwrap();
    assert_eq!(num(&db, "S"), 0.0);
    assert_eq!(num(&db, "S.STAT"), DISABLE);
    assert_eq!(num(&db, "R.PACT"), 0.0);
}

#[epics_macros_rs::epics_test]
async fn a_fanout_link_holds_the_fanout_pact() {
    let db = ioc().await;
    db.process_record("FO").await.unwrap();
    assert_eq!(num(&db, "T1"), 0.0);
    assert_eq!(num(&db, "T1.STAT"), DISABLE);
    assert_eq!(num(&db, "FO.PACT"), 0.0);
}
