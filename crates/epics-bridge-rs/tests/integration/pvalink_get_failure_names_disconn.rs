//! A read through a pva link that is not valid takes pvalink's own message:
//! `pvaGetValue` raises `"%s Disconn"` (`pvalink_lset.cpp:263-268`) before
//! `dbGetLink`'s `setLinkAlarm`, whose equal severity leaves it in place
//! (`dbLink.c:335-339`, `recGbl.c:242`). A put has no such message, so the
//! output keeps `setLinkAlarm`'s `"field OUT"`. softIocPVX, unconnected:
//!
//! ```text
//! PL:AI   AMSG "INP Disconn"
//! PL:AO   AMSG "field OUT"
//! ```
#![cfg(feature = "pvalink")]

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// suite.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;

const DB: &str = r#"
record(ai, "PL:AI") { field(INP, {pva:"PL:NOPE"}) }
record(calc, "PL:CALC") { field(INPA, {pva:"PL:NOPE"}) field(CALC, "A") }
record(ao, "PL:AO") { field(OUT, {pva:"PL:NOPE"}) }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_pva_link_names_disconn_on_read_and_the_field_on_put() {
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |db| async move {
            epics_bridge_rs::pvalink::install_pvalink_resolver(&db).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;

    for (rec, amsg) in [
        ("PL:AI", "INP Disconn"),
        ("PL:CALC", "INPA Disconn"),
        ("PL:AO", "field OUT"),
    ] {
        db.process_record(rec)
            .await
            .or_else(epics_base_rs::error::CaError::unless_process_status)
            .unwrap();
        let cell = db.get_record(rec).unwrap();
        let inst = cell.read();
        assert_eq!(inst.common.sevr, AlarmSeverity::Invalid, "{rec}");
        assert_eq!(inst.common.stat, 14, "{rec} STAT LINK");
        assert_eq!(inst.common.amsg, amsg, "{rec}");
    }
}
