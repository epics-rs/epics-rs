//! `aSubRecord.c:414` posts VAL only on `val != oval`, so an alarm transition
//! with VAL unchanged posts SEVR and no VAL. softIoc, `camonitor -m va AS
//! AS.SEVR` across UDF -> NO_ALARM -> MAJOR LINK -> NO_ALARM: three SEVR
//! events, no VAL event past the initial one.

use std::collections::HashMap;

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[epics_macros_rs::epics_test]
async fn an_alarm_transition_with_val_unchanged_posts_no_val() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(ai, "SRC") { field(VAL, "0") field(HIHI, "5") field(HHSV, "MAJOR") }
record(aSub, "AS") { field(INPA, "SRC MS") field(FTA, "DOUBLE") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    let (mut val, mut sevr) = {
        let rec = db.get_record("AS").unwrap();
        let mut g = rec.write();
        (
            g.add_subscriber("VAL", 1, DbFieldType::Long, mask).unwrap(),
            g.add_subscriber("SEVR", 2, DbFieldType::Short, mask)
                .unwrap(),
        )
    };
    let mut sevr_events = 0;
    for src in [None, Some(10.0), Some(0.0)] {
        if let Some(v) = src {
            db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(v))
                .await
                .unwrap();
        }
        db.process_record_with_links("AS", &mut ProcStack::new())
            .await
            .unwrap();
        while sevr.try_recv().is_ok() {
            sevr_events += 1;
        }
    }
    let mut val_events = 0;
    while val.try_recv().is_ok() {
        val_events += 1;
    }
    assert_eq!(sevr_events, 3, "SEVR transitions");
    assert_eq!(val_events, 0, "VAL events on alarm-only cycles");
}
