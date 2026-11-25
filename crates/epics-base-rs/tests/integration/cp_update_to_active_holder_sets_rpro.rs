//! A CA CP update processes its holder through `db_process` (dbCa.c:1320,
//! db_access.c:1046-1057): an active holder takes RPRO and is processed
//! again when its cycle ends, without dbProcess's LCNT count.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "S") { field(VAL, "0") }
record(calcout, "CO") { field(INPA, "S CP") field(CALC, "A") field(ODLY, "0.5") }
"#;

fn get(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn cp_update_to_active_holder_sets_rpro() {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    let budget = fact_budget::FACT_BUDGET;
    // The connect-time process has ended its ODLY.
    let idle = async {
        while get(&db, "CO.PACT") != 0.0 {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
    };
    epics_base_rs::runtime::task::timeout(budget, idle)
        .await
        .unwrap();

    db.put_pv_and_post("S", EpicsValue::Double(1.0))
        .await
        .unwrap();
    db.ca_sync().await;
    assert_eq!(get(&db, "CO.PACT"), 1.0, "CO waits out its ODLY");
    db.put_pv_and_post("S", EpicsValue::Double(2.0))
        .await
        .unwrap();
    db.ca_sync().await;
    assert_eq!(get(&db, "CO.RPRO"), 1.0, "db_process marks the active CO");
    assert_eq!(
        get(&db, "CO.LCNT"),
        0.0,
        "db_process does not call dbProcess"
    );

    let reprocessed = async {
        while get(&db, "CO.VAL") != 2.0 {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
    };
    epics_base_rs::runtime::task::timeout(budget, reprocessed)
        .await
        .expect("RPRO reprocesses CO with the second update");
}
