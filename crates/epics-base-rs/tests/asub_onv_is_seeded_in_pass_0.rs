//! `aSubRecord.c:118-121` sizes every channel in pass 0, and `initFields`
//! sets `ONVx` to `NOVx` there (`:197-203`), before the SNAM lookup that
//! can return early (`:153-160`). softIoc reads `ONVA` 80 for `NOVA 80`
//! with an SNAM that does not resolve.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn onva_is_nova_when_snam_does_not_resolve() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(aSub, "BAD") { field(SNAM, "noSuch") field(FTVA, "CHAR") field(NOVA, "80") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("BAD.ONVA").ok().and_then(|v| v.to_f64()),
        Some(80.0)
    );
}
