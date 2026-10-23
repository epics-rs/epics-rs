//! C `dbCaSync` (`dbCa.c:283`) waits for the work queued to local links
//! before the call, not for the links to go idle: a CP cycle posting
//! forever must not hold it.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "A") { field(INPA, "B CP") field(CALC, "A+1") }
record(calc, "B") { field(INPA, "A CP") field(CALC, "A+1") }
record(ao, "T") { field(VAL, "1") }
record(calc, "H") { field(INPA, "T CP") field(CALC, "A") }
"#;

fn val(db: &epics_base_rs::server::database::PvDatabase, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn ca_sync_returns_with_a_cp_cycle_running() {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    let a0 = val(&db, "A");
    let cycling = async {
        while val(&db, "A") < a0 + 100.0 {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
    };
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, cycling)
        .await
        .expect("the A<->B cycle runs");
    db.put_pv_and_post("T", EpicsValue::Double(5.0))
        .await
        .unwrap();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, db.ca_sync())
        .await
        .expect("ca_sync returns while the A<->B cycle runs");
    assert_eq!(
        val(&db, "H"),
        5.0,
        "T's post, queued before the call, is processed"
    );
}
