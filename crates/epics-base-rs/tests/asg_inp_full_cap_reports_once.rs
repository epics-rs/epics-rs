//! An ASG `INP*` link whose record has a full subscriber cap is reported with
//! asCa.c:79's `exceptionCallback` line, once: C's `ca_add_event` is issued
//! once per input, so its `ECA_ALLOCMEM` exception arrives once, while the
//! port retries the attach on every rebuild.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::access_security::{
    new_acf_cell_watching, notify_asg_field_changed, parse_acf,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::DbFieldType;

#[epics_macros_rs::epics_test]
async fn a_full_cap_on_an_asg_input_prints_ascas_exception_once() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("CAPR", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("CAPR").unwrap();
    let held: Vec<_> = (1..)
        .map_while(|sid| {
            rec.write()
                .add_subscriber("VAL", sid, DbFieldType::Double, 1)
                .ok()
        })
        .collect();
    assert!(!held.is_empty());

    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = heard.clone();
    errlog_add_listener(move |m| {
        if m.starts_with("dbCa:") {
            sink.lock().unwrap().push(m.to_string());
        }
    });
    let acf = parse_acf(
        r#"ASG(DEFAULT) { RULE(1, READ) }
           ASG(G) { INPA("CAPR.VAL") RULE(1, WRITE) { CALC("A") } }"#,
    )
    .unwrap();
    let _cell = new_acf_cell_watching(Some(acf), &db);
    sleep(Duration::from_millis(200)).await;
    // Each notification is a rebuild, and every rebuild retries the attach.
    for _ in 0..2 {
        notify_asg_field_changed();
        sleep(Duration::from_millis(200)).await;
    }
    errlog_flush();
    assert_eq!(
        *heard.lock().unwrap(),
        [
            "dbCa:exceptionCallback stat \"Unable to allocate additional dynamic memory\" \
             channel \"CAPR.VAL\" context \"subscription install into record CAPR failed\"\n \
             nativeType DBR_DOUBLE requestType DBR_STS_DOUBLE nativeCount 1 requestCount 0 \
             readAccess noWriteAccess\n"
        ]
    );
}
