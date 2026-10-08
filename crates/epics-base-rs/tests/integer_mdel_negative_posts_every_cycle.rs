//! `MDEL < 0` posts `DBE_VALUE` on every cycle, VAL moved or not:
//! longinRecord.c and int64inRecord.c open `monitor()` with
//! `if (prec->mdel < 0 || DELTA(prec->mlst, prec->val) > prec->mdel)`
//! (int64inRecord.c:370-371). softIoc 7.0.10 on `field(VAL,"5")
//! field(MDEL,"-1")`: `camonitor -m v` printed `5` on each of three
//! `caput .PROC 1`, for longin and int64in alike.

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

fn value_posts(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while let Ok(ev) = rx.try_recv() {
        if ev.mask.contains(EventMask::VALUE) {
            n += 1;
        }
    }
    n
}

#[epics_macros_rs::epics_test]
async fn mdel_minus_one_posts_an_unchanged_val_every_cycle() {
    let db = ioc(r#"record(longin, "LI") {
    field(VAL, "5")
    field(MDEL, "-1")
}
record(int64in, "I64") {
    field(VAL, "5")
    field(MDEL, "-1")
}"#)
    .await;

    for name in ["LI", "I64"] {
        let mut rx = subscribe_value(&db, name);
        for cycle in 1..=3 {
            db.process_record_with_links(name, &mut ProcStack::new())
                .await
                .unwrap();
            assert_eq!(
                value_posts(&mut rx),
                1,
                "{name} cycle {cycle}: MDEL=-1 posts DBE_VALUE with VAL unchanged"
            );
        }
    }
}
