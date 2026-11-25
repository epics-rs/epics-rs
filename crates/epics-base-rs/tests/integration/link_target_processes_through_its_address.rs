//! A fanout `LNKn` and a `PP` OUT link process the record their cached
//! address names — C `processTarget` on the `dbAddr`'s `precord` — so an alias
//! reaches its record and the Passive gate still holds. Every expectation is
//! softIoc's `dbgf` on the same database.

use epics_base_rs::server::ioc_builder::IocBuilder;
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(fanout, "FA") { field(LNK1, "CN") field(LNK2, "EV") }
record(calc, "CREAL") { field(CALC, "VAL+1") alias("CN") }
record(calc, "EV") { field(SCAN, "Event") field(EVNT, "77") field(CALC, "VAL+1") }
record(ao, "PO") { field(VAL, "2") field(OUT, "KN.A PP") }
record(calc, "K") { field(CALC, "A*10") alias("KN") }
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

/// softIoc: CREAL 1 through the alias, EV 0 — `dbScanPassive` refuses a
/// non-Passive target.
#[epics_macros_rs::epics_test]
async fn a_fanout_processes_an_aliased_passive_target_only() {
    let db = build().await;
    db.process_record("FA").await.unwrap();
    assert_eq!((val(&db, "CREAL"), val(&db, "EV")), (1.0, 0.0));
}

/// softIoc: K 20 — the PP write processed the record behind the alias.
#[epics_macros_rs::epics_test]
async fn a_pp_out_link_processes_the_record_behind_an_alias() {
    let db = build().await;
    db.process_record("PO").await.unwrap();
    assert_eq!(val(&db, "K"), 20.0);
}
