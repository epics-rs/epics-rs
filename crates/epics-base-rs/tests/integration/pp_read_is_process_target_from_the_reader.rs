//! A PP input read processes its source as C `dbDbGetValue`'s
//! `dbScanPassive(precord, …)` (`dbDbLink.c:175`) does: `processTarget` with
//! the reader as `psrc` (`:436-528`), so the source takes the reader's PUTF,
//! a busy source gets RPRO only from a put-triggered reader, and the source
//! joins the reader's put-notify (`dbNotifyAdd`, `:459-460`).

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ProcessCompletion;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "S") { field(CALC, "A") field(INPA, "S.PUTF") }
record(calc, "R") { field(CALC, "A") field(INPA, "S PP") }
record(calcout, "BUSY") { field(CALC, "A") field(ODLY, "0.3") }
record(calc, "RB") { field(CALC, "A") field(INPA, "BUSY PP") }
record(calcout, "BUSY2") { field(CALC, "A") field(ODLY, "0.3") }
record(calc, "RB2") { field(CALC, "A") field(INPA, "BUSY2 PP") }
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

async fn wait_idle(db: &PvDatabase, rec: &str) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        if num(db, &format!("{rec}.PACT")) == 0.0 {
            return;
        }
    }
    panic!("{rec} still PACT");
}

#[epics_macros_rs::epics_test]
async fn the_source_takes_the_reader_putf() {
    let db = ioc().await;
    db.put_record_field_from_ca("R", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(num(&db, "S"), 1.0, "S processed with the reader's PUTF");
    assert_eq!(num(&db, "R"), 1.0);

    db.process_record("R").await.unwrap();
    assert_eq!(num(&db, "S"), 0.0, "a scanned reader hands no PUTF");
    assert_eq!(num(&db, "R"), 0.0);
}

#[epics_macros_rs::epics_test]
async fn a_busy_source_gets_rpro_only_from_a_put_reader() {
    let db = ioc().await;
    db.put_record_field_from_ca("BUSY", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(num(&db, "BUSY.PACT"), 1.0, "BUSY waits out its ODLY");
    db.put_record_field_from_ca("RB", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(num(&db, "BUSY.RPRO"), 1.0);
    wait_idle(&db, "BUSY").await;
    wait_idle(&db, "BUSY").await;
    assert_eq!(num(&db, "BUSY.RPRO"), 0.0);

    db.put_record_field_from_ca("BUSY2", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(num(&db, "BUSY2.PACT"), 1.0);
    db.process_record("RB2").await.unwrap();
    assert_eq!(num(&db, "BUSY2.RPRO"), 0.0, "a scanned reader sets no RPRO");
    wait_idle(&db, "BUSY2").await;
}

#[epics_macros_rs::epics_test]
async fn the_source_joins_the_reader_put_notify() {
    let db = ioc().await;
    let ProcessCompletion::Async(done) = db.process_record_with_notify("RB").await.unwrap() else {
        panic!("the put-notify must wait for the PP source's ODLY");
    };
    assert_eq!(num(&db, "BUSY.PACT"), 1.0);
    let ended = epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, done)
        .await
        .expect("the put-notify never ended");
    assert!(ended.is_ok(), "the put-notify completed");
    assert_eq!(num(&db, "BUSY.PACT"), 0.0, "and only after BUSY did");
}
