//! `aiRecord.c:462-465` posts RVAL under `oraw != rval` and then advances
//! ORAW without posting it. softIoc: `camonitor ai.ORAW ai.RVAL` across
//! three processes with RVAL 1, 5, 9 shows RVAL three times and ORAW only
//! its connect value.

use std::collections::HashMap;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn rval_posts_and_oraw_does_not() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(longout, "SRC") { field(VAL, "1") }
record(ai, "AI") { field(DTYP, "Raw Soft Channel") field(INP, "SRC") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let mask = (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    let ai = db.get_record("AI").unwrap();
    let mut oraw = ai
        .write()
        .add_subscriber("ORAW", 1, DbFieldType::Long, mask)
        .unwrap();
    let mut rval = ai
        .write()
        .add_subscriber("RVAL", 2, DbFieldType::Long, mask)
        .unwrap();

    // The connect value, as camonitor's first line.
    while oraw.try_recv().is_ok() {}
    while rval.try_recv().is_ok() {}

    for v in [1, 5, 9] {
        db.get_record("SRC")
            .unwrap()
            .write()
            .record
            .put_field("VAL", EpicsValue::Long(v))
            .unwrap();
        process(&db, "AI").await;
        assert!(rval.try_recv().is_ok(), "RVAL {v} posts");
        while rval.try_recv().is_ok() {}
        assert!(oraw.try_recv().is_err(), "ORAW {v} must not post");
    }
    assert_eq!(
        ai.read().record.get_field("ORAW"),
        Some(EpicsValue::Long(9))
    );
}
