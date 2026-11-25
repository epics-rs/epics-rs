//! A database built by `PvaServer::builder` serves its own CP links with no
//! `ca` link set installed (this crate has none), as C's does: `dbCa` and `dbContext` are in base.
#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_pva_rs::server::PvaServer;

const DB: &str = r#"
record(ao, "LB:SRC") { field(VAL, "2") }
record(calc, "LB:H") { field(INPA, "LB:SRC CP") field(CALC, "A") }
"#;

async fn until(db: &PvDatabase, pv: &str, want: f64) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while db.get_pv(pv).unwrap().to_f64() != Some(want) {
        assert!(Instant::now() < deadline, "{pv} never reached {want}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
}

#[epics_macros_rs::epics_test]
async fn a_local_cp_link_processes_on_source_change() {
    let server = PvaServer::builder()
        .port(0)
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();
    // The IOC entry's `scanRun` and `dbCaRun`; a protocol server starts none.
    let _scan = ScanOwner::start(db.clone());
    until(&db, "LB:H", 2.0).await;
    db.put_record_field_from_ca("LB:SRC", "VAL", EpicsValue::Double(7.0))
        .await
        .unwrap();
    until(&db, "LB:H", 7.0).await;
}
