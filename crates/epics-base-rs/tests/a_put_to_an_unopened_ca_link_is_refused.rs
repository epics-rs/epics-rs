//! C opens every CA link at `dbCaAddLink` and `dbCaPutLinkCallback` refuses a
//! put while it is not connected (`dbCa.c:529-532`), which `dbPutLink` turns
//! into LINK/INVALID (`dbLink.c:443-446`). A link not opened yet is not
//! connected: its put is refused, not staged to open the link.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn a_put_before_the_link_opens_alarms_and_writes_nothing() {
    let (db, _h) = IocBuilder::new()
        .db_string(
            r#"
record(ao, "SRC") { field(OUT, "TGT CA") }
record(ao, "TGT") { field(VAL, "7") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    // No iocInit open pass: the link is not opened.
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;

    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    let num = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    assert_eq!(num("SRC.STAT"), 14.0, "LINK");
    assert_eq!(num("SRC.SEVR"), 3.0, "INVALID");
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    assert_eq!(num("TGT"), 7.0, "the refused put wrote TGT");
}
