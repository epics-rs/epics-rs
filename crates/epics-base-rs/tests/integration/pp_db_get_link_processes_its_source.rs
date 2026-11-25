//! Every `dbGetLink` / `dbTryGetLink` of a `PP` DB link processes its passive
//! source first: `dbDbGetValue` runs `dbScanPassive` for whichever field the
//! link is (`dbDbLink.c:174-179`), so TSEL (`recGbl.c:322`), SDIS
//! (`dbAccess.c:566`), SIML (`recGbl.c:453`), SIOL and SELL
//! (`seqRecord.c:152`) all do; an `NPP` link does not.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(calc, "TSRC") { field(CALC, "VAL+1") }
record(ai, "T") { field(TSEL, "TSRC PP") }
record(calc, "TNSRC") { field(CALC, "VAL+1") }
record(ai, "TN") { field(TSEL, "TNSRC NPP") }
record(calc, "DSRC") { field(CALC, "VAL+1") }
record(ai, "D") { field(SDIS, "DSRC PP") field(DISV, "5") }
record(calc, "MSRC") { field(CALC, "VAL+1") }
record(ai, "M") { field(SIML, "MSRC PP") }
record(calc, "OSRC") { field(CALC, "VAL+1") }
record(ai, "O") { field(SIMM, "YES") field(SIOL, "OSRC PP") }
record(calc, "QSRC") { field(CALC, "VAL+1") }
record(seq, "Q") { field(SELM, "Specified") field(SELL, "QSRC PP") }
"#;

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_pp_tsel_processes_its_source() {
    let db = ioc().await;
    db.process_record("T").await.unwrap();
    assert_eq!(num(&db, "TSRC"), 1.0);
    db.process_record("TN").await.unwrap();
    assert_eq!(num(&db, "TNSRC"), 0.0, "an NPP TSEL only reads");
}

#[epics_macros_rs::epics_test]
async fn a_pp_sdis_processes_its_source() {
    let db = ioc().await;
    db.process_record("D").await.unwrap();
    assert_eq!(num(&db, "DSRC"), 1.0);
}

#[epics_macros_rs::epics_test]
async fn a_pp_siml_processes_its_source() {
    let db = ioc().await;
    db.process_record("M").await.unwrap();
    assert_eq!(num(&db, "MSRC"), 1.0);
}

#[epics_macros_rs::epics_test]
async fn a_pp_siol_processes_its_source() {
    let db = ioc().await;
    db.process_record("O").await.unwrap();
    assert_eq!(num(&db, "OSRC"), 1.0);
}

#[epics_macros_rs::epics_test]
async fn a_pp_sell_processes_its_source() {
    let db = ioc().await;
    db.process_record("Q").await.unwrap();
    assert_eq!(num(&db, "QSRC"), 1.0);
}
