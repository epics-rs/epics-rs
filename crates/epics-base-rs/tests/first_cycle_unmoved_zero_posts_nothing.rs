//! A record whose dset init succeeds is seeded `mlst = val`
//! (aoRecord.c:156-159, longinRecord.c:120-122), so a VAL of 0 that the
//! first cycle leaves at 0 is inside MDEL = 0 and posts no `DBE_VALUE`.
//! softIoc 7.0.10 on `field(VAL,"0")`: `camonitor -m v` printed nothing
//! after `caput .PROC 1` on ao and longin, and `.MLST` read 0 for both.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::DbFieldType;

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

fn subscribe_value(db: &PvDatabase, name: &str) -> EventReader {
    let rec = db.get_record(name).unwrap();
    let mut inst = rec.write();
    inst.add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .expect("subscription accepted")
}

/// Alarm-only posts (the first cycle's UDF clear) carry no `DBE_VALUE` and
/// are what `camonitor -m v` drops too.
fn value_posts(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while let Ok(ev) = rx.try_recv() {
        if ev.mask().contains(EventMask::VALUE) {
            n += 1;
        }
    }
    n
}

#[epics_macros_rs::epics_test]
async fn an_unmoved_zero_posts_no_value_on_the_first_cycle() {
    let db = ioc(r#"record(ao, "AO") {
    field(VAL, "0")
}
record(longin, "LI") {
    field(VAL, "0")
}"#)
    .await;

    for name in ["AO", "LI"] {
        let mut rx = subscribe_value(&db, name);
        db.process_record_with_links(name, &mut ProcStack::new())
            .await
            .unwrap();
        assert_eq!(
            value_posts(&mut rx),
            0,
            "{name}: VAL 0 against the seeded MLST 0 with MDEL 0"
        );
    }
}
