//! `db_event_enable` / `db_event_disable` flip a monitor in the entry every
//! reader shares (`dbEvent.c:524-535`): a rights change taken alongside a
//! reader of the same entry is seen by that reader, with no list rebuilt.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::DbFieldType;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(
            "record(calc, \"C\") { field(CALC, \"A\") }\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

#[epics_macros_rs::epics_test]
async fn a_rights_change_beside_a_reader_flips_the_entry_it_holds() {
    let db = ioc().await;
    let rec = db.get_record("C").unwrap();
    let mut reader = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .expect("subscribe");
    let id = reader.id();

    for (active, posts) in [(false, false), (true, true)] {
        {
            // One hold, shared by both threads: the setter has only `&` to
            // the record, as the rights path will have to a published list.
            let held = rec.read();
            let inst: &epics_base_rs::server::record::RecordInstance = &held;
            let seen = inst.subscribers_of("VAL").next().expect("the monitor");
            std::thread::scope(|s| {
                s.spawn(|| inst.set_subscriber_active(id, active));
                while seen.is_active() != active {
                    std::hint::spin_loop();
                }
            });
            assert!(
                std::ptr::eq(seen, held.subscribers_of("VAL").next().unwrap()),
                "the entry was replaced"
            );
        }
        while reader.try_recv().is_ok() {}
        {
            let mut inst = rec.write();
            inst.notify_field("VAL", EventMask::VALUE);
        }
        assert_eq!(reader.try_recv().is_ok(), posts, "active={active}");
    }
}
