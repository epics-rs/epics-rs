//! A PP link whose target's `process()` fails raises LINK/INVALID on the
//! record that followed it: `dbDbPutValue` returns `processTarget`'s status
//! (`dbDbLink.c:387-392`) and `dbDbGetValue` returns `dbScanPassive`'s
//! before reading (`dbDbLink.c:175-178`), and `dbPutLink`/`dbGetLink` call
//! `setLinkAlarm` on any non-zero status. Measured on softIoc with the
//! records below: each shows INVALID/LINK with AMSG naming its link.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{LinkPutOp, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Every remote channel down: `dbCa`'s -1 (`dbCa.c:558-560`).
struct Down;

#[async_trait::async_trait]
impl LinkSet for Down {
    fn is_connected(&self, _: &[u8]) -> bool {
        false
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: &[u8], _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Err("channel disconnected".into())
    }
}

const DB: &str = r#"
record(ao, "F:ao") { field(OUT, "NOPV:X CA") }
record(ai, "F:ai") { field(INP, "NOPV:X CA") }
record(ao, "F:aoPP") { field(OUT, "F:ao PP") }
record(ai, "F:aiPP") { field(INP, "F:ai PP") }
record(calc, "F:calcPP") { field(INPA, "F:ai PP") field(CALC, "A") }
record(ao, "F:dolPP") { field(DOL, "F:ai PP") field(OMSL, "closed_loop") }
"#;

#[epics_macros_rs::epics_test]
async fn a_failing_pp_target_alarms_the_link_holder() {
    let db = Arc::new(PvDatabase::new());
    db.register_link_set("ca", Arc::new(Down)).await;
    let (db, _) = IocBuilder::new()
        .with_database(db)
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let mut wrong = Vec::new();
    for (name, link) in [
        ("F:aoPP", "OUT"),
        ("F:aiPP", "INP"),
        ("F:calcPP", "INPA"),
        ("F:dolPP", "DOL"),
    ] {
        let _ = db
            .put_record_field_from_ca_no_notify(name, "PROC", EpicsValue::Long(1))
            .await;
        let get = |f: &str| db.get_pv(format!("{name}.{f}")).unwrap().to_string();
        let got = (get("SEVR"), get("STAT"), get("AMSG"));
        if got != ("3".into(), "14".into(), format!("field {link}")) {
            wrong.push(format!("{name}: {got:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "INVALID/LINK, AMSG naming the link: {wrong:#?}"
    );
}
