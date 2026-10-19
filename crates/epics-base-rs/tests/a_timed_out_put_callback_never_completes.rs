//! A put callback the local channel could not issue is never completed:
//! `dbCaTask` logs a failed `ca_array_put_callback` and leaves
//! `newOutNative` set, but nothing ever calls `putComplete` for it
//! (`dbCa.c:1233-1247`). The way it fails on a local channel is the
//! `dbPutNotifyBlocker` giving up after 30 s (`dbPutNotifyBlocker.cpp:146-149`).

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Both outputs reach TGT through one local channel, so OUT2's put callback
/// waits on the blocker OUT1's holds for TGT's 40 s delay, and times out.
const DB: &str = r#"
record(ao, "OUT1") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT.A CA")
}
record(ao, "OUT2") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT.A CA")
}
record(calcout, "TGT") { field(CALC, "1") field(ODLY, "40") }
"#;

#[epics_macros_rs::epics_test]
async fn a_put_callback_whose_blocker_timed_out_leaves_its_record_active() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;

    db.put_record_field_from_ca("OUT1", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    let tgt = db.get_record("TGT").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !tgt.read().is_processing() && std::time::Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    assert!(tgt.read().is_processing(), "OUT1's put started TGT's delay");

    db.put_record_field_from_ca("OUT2", "VAL", EpicsValue::Double(2.0))
        .await
        .unwrap();
    let out2 = db.get_record("OUT2").unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_secs(32)).await;

    assert!(tgt.read().is_processing(), "TGT is still in its delay");
    assert!(
        out2.read().is_processing(),
        "OUT2 completed a put callback that was never issued"
    );
}
