//! Every `caLink` opens its own channel, so its own `dbPutNotifyBlocker`
//! (`dbContext.cpp`, `dbPutNotifyBlocker.cpp:132-176`): a second link's put
//! callback to a PV another link is still waiting on is issued at once, and
//! dbNotify restarts the target for it when the first processing ends
//! (`dbNotify.c` `notifyWaitForRestart`). Only a blocker shared between the
//! two links could make it wait, and give up after 30 s
//! (`dbPutNotifyBlocker.cpp:146-149`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "OUT1") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT.A CA")
}
record(ao, "OUT2") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT.A CA")
}
record(calcout, "TGT") { field(CALC, "A") field(ODLY, "31") }
"#;

#[epics_macros_rs::epics_test]
async fn a_second_links_put_callback_restarts_the_target_past_the_blocker_timeout() {
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
    open_links(&db).await;

    db.put_record_field_from_ca("OUT1", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    let tgt = db.get_record("TGT").unwrap();
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !tgt.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    assert!(tgt.read().is_processing(), "OUT1's put started TGT's delay");
    let first = Instant::now();

    db.put_record_field_from_ca("OUT2", "VAL", EpicsValue::Double(2.0))
        .await
        .unwrap();
    let (out1, out2) = (
        db.get_record("OUT1").unwrap(),
        db.get_record("OUT2").unwrap(),
    );
    let deadline = first + Duration::from_secs(40);
    while out1.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    epics_base_rs::runtime::task::sleep(Duration::from_secs(1)).await;

    assert!(
        !out1.read().is_processing(),
        "OUT1 completed with TGT's first pass"
    );
    assert!(
        tgt.read().is_processing(),
        "OUT2's put restarted TGT once its first pass ended"
    );
    assert_eq!(db.get_pv("TGT.A").unwrap(), EpicsValue::Double(2.0));
    assert!(out2.read().is_processing(), "OUT2 waits for that pass");
}

/// iocInit's open pass, then each external link connected: C opens every CA
/// link at `dbCaAddLink` and refuses its puts until it connects.
async fn open_links(db: &epics_base_rs::server::database::PvDatabase) {
    db.setup_external_link_opens().await;
    for record in db.all_record_names() {
        for (field, _, link) in db.record_link_fields(&record) {
            let Some(link) = link.external() else {
                continue;
            };
            let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
            while db.external_link_metadata(link).is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{record}.{field} connects"
                );
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    }
}
