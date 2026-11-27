//! A CA CP update processes an idle holder with PUTF set (`db_process`,
//! db_access.c:1046-1057), and `processTarget` copies it down the holder's
//! FLNK (dbDbLink.c:468-474), so an async target keeps PUTF while PACT.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "S") { field(VAL, "0") }
record(calc, "H") { field(INPA, "S CP") field(CALC, "A") field(FLNK, "T") }
record(calcout, "T") { field(CALC, "1") field(ODLY, "0.5") }
"#;

fn get(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn cp_update_sets_putf_on_an_idle_holder() {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    // The connect-time process has ended T's ODLY.
    let idle = async {
        while get(&db, "T.PACT") != 0.0 {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
    };
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, idle)
        .await
        .unwrap();

    db.put_pv_and_post("S", EpicsValue::Double(1.0))
        .await
        .unwrap();
    db.ca_sync().await;
    assert_eq!(get(&db, "T.PACT"), 1.0, "T waits out its ODLY");
    assert_eq!(get(&db, "T.PUTF"), 1.0, "processTarget copied H's PUTF");
}
