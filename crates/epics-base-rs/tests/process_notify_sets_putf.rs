//! `processNotifyCommon` sets PUTF on a record it reaches first
//! (dbNotify.c:252-255), so a `dbtpn` with no value hands PUTF down the
//! chain through `processTarget` like any put does.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(calc, "H") { field(FLNK, "T") }
record(calcout, "T") { field(ODLY, "0.5") field(CALC, "1") }
"#;

#[epics_macros_rs::epics_test]
async fn a_process_notify_hands_putf_to_its_flnk_target() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let _completion = db.process_record_with_notify("H").await.unwrap();
    let t = db.get_record("T").unwrap();
    let t = t.read();
    assert!(t.is_processing(), "H's FLNK started T's ODLY");
    assert!(t.common.putf, "processTarget copied H's PUTF");
}
