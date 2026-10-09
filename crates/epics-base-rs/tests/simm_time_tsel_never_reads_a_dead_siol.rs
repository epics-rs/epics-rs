//! A `.TIME` TSEL returns from `recGblGetTimeStampSimm` before the SIOL arm
//! (`recGbl.c:321`), so a dead SIOL is never looked at and nothing is
//! errlogged; the same record without that TSEL reports it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB_TEXT: &str = r#"
record(ai, "TT_SRC") {}
record(ai, "TT_TIME") { field(SIMM, "YES") field(SIOL, "TT_NOSUCH") field(TSE, "-2") field(TSEL, "TT_SRC.TIME") }
record(ai, "TT_PLAIN") { field(SIMM, "YES") field(SIOL, "TT_NOSUCH") field(TSE, "-2") }
"#;

#[epics_macros_rs::epics_test]
async fn a_time_tsel_skips_the_siol_stamp_read() {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().expect("sink").push(m.to_string());
    });
    let db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for rec in ["TT_TIME", "TT_PLAIN"] {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links(rec, &mut visited)
            .await
            .unwrap();
    }
    epics_base_rs::runtime::log::errlog_flush();
    let heard = heard.lock().expect("sink").join("\n");
    assert!(
        heard.contains("TT_PLAIN.SIOL = TT_NOSUCH"),
        "the control record reports its dead SIOL: {heard:?}"
    );
    assert!(!heard.contains("TT_TIME"), "{heard:?}");
}
