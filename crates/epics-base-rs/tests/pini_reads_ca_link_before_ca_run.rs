//! PINI (iocInit.c:196) runs before dbCaRun (:260), and dbCaTask services no
//! connect before it, so a PINI read through a CA link finds it unconnected
//! (dbCa.c:459) and raises LINK/INVALID, however long the link has had to
//! connect. The CP connect then clears SEVR; ACKS keeps the INVALID.

mod calink_ioc;

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(ao, "S") { field(VAL, "5") }
record(longout, "TW") {
    field(PINI, "YES")
    field(DOL, "S CP")
    field(OMSL, "closed_loop")
}
"#;

fn get(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn pini_reads_ca_link_before_ca_run() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    // Give the link every chance to connect before PINI.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    let _owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    db.wait_for_pini().await;
    calink_ioc::until("the CP connect processes TW", || get(&db, "TW.VAL") == 5.0).await;
    db.ca_sync().await;
    assert_eq!(get(&db, "TW.SEVR"), 0.0, "the CP connect clears the alarm");
    assert_eq!(get(&db, "TW.ACKS"), 3.0, "PINI read an unconnected link");
}
