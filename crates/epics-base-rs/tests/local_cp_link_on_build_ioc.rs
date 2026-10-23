//! An IOC built by `IocBuilder::build_ioc` — `realtime-pva-ioc`, `qsrv-ioc`
//! — serves its own CP links with no `ca` link set installed.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

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
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    until(&db, "LB:H", 2.0).await;
    db.put_record_field_from_ca("LB:SRC", "VAL", EpicsValue::Double(7.0))
        .await
        .unwrap();
    until(&db, "LB:H", 7.0).await;
}
