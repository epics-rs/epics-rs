//! C `dbPutFieldLink` after `initDatabase` reaches `dbAddLink` →
//! `dbCaAddLink`, which queues the new link's `CA_CONNECT` whether or not
//! `dbCaRun` has run (`dbCa.c:397-401`); `dbCaRun` then services it. A link
//! relinked in between is therefore open once `dbCaRun` returns, and a put
//! through it lands.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn a_put_through_a_link_relinked_before_ca_run_lands() {
    let (db, _h) = IocBuilder::new()
        .db_string(
            r#"
record(ao, "SRC") { field(OUT, "TGT CA") }
record(ao, "TGT") { field(VAL, "7") }
record(ao, "TGT2") { field(VAL, "7") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    // iocInit's link passes, then a relink before dbCaRun.
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.setup_external_link_opens().await;
    db.put_record_field_from_ca("SRC", "OUT", EpicsValue::String("TGT2 CA".into()))
        .await
        .unwrap();
    db.scan_control().run();
    db.process_cp_links_on_connect().await;

    let num = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(5.0))
            .await
            .unwrap();
        if num("TGT2") == 5.0 || std::time::Instant::now() > deadline {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(num("TGT2"), 5.0, "the relinked link never opened");
    assert_eq!(num("SRC.SEVR"), 0.0, "NO_ALARM");
    assert_eq!(num("TGT"), 7.0, "the old link still wrote");
}
